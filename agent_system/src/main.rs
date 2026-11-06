use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use tokio::sync::watch;
use tonic::transport::{Identity, Server, ServerTlsConfig};

use aetherlynx_orchestrator::auth::TokenAuth;
use aetherlynx_orchestrator::integration::HealthProbe;
use aetherlynx_orchestrator::proto::aether_lynx_server::AetherLynxServer;
use aetherlynx_orchestrator::reconciler::Reconciler;
use aetherlynx_orchestrator::registry::Registry;
use aetherlynx_orchestrator::runtime::SimulatedRuntime;
use aetherlynx_orchestrator::service::AetherLynxService;

#[derive(Parser)]
#[command(
    name = "aetherlynx-orchestrator",
    version,
    about = "Orquestador de agentes LLM de AetherLynx"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inicia el servidor gRPC y el reconciliador
    Start(StartArgs),
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Args)]
struct StartArgs {
    /// Dirección de escucha
    #[arg(long, env = "AETHERLYNX_LISTEN", default_value = "127.0.0.1:50051")]
    listen: SocketAddr,

    /// Token bearer exigido a los clientes (mínimo 32 caracteres)
    #[arg(long, env = "AETHERLYNX_API_TOKEN", hide_env_values = true)]
    api_token: Option<String>,

    /// Desactiva la autenticación. Solo se permite escuchando en loopback.
    #[arg(long)]
    allow_unauthenticated: bool,

    /// Certificado TLS del servidor (PEM)
    #[arg(long, env = "AETHERLYNX_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// Clave privada TLS del servidor (PEM)
    #[arg(long, env = "AETHERLYNX_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// Permite escuchar fuera de loopback sin TLS (p. ej. detrás de un service mesh con mTLS)
    #[arg(long)]
    allow_insecure_transport: bool,

    /// Intervalo entre ciclos de reconciliación
    #[arg(long, default_value = "5s", value_parser = humantime::parse_duration)]
    reconcile_interval: Duration,

    /// Réplicas que el runtime simulado sube o baja por ciclo
    #[arg(long, default_value_t = 1)]
    scale_step: u32,

    /// Endpoint de salud del sistema transaccional (https), opcional
    #[arg(long, env = "AETHERLYNX_INTEGRATION_URL")]
    integration_url: Option<String>,

    /// Intervalo de la sonda de integración
    #[arg(long, default_value = "30s", value_parser = humantime::parse_duration)]
    integration_interval: Duration,

    /// Formato de log
    #[arg(
        long,
        env = "AETHERLYNX_LOG_FORMAT",
        value_enum,
        default_value = "text"
    )]
    log_format: LogFormat,
}

fn init_logging(format: LogFormat) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

/// Valida la combinación de opciones de seguridad antes de abrir el puerto.
fn security_config(a: &StartArgs) -> Result<TokenAuth, String> {
    let loopback = a.listen.ip().is_loopback();
    let auth = match (&a.api_token, a.allow_unauthenticated) {
        (Some(_), true) => {
            return Err("--api-token y --allow-unauthenticated son incompatibles".into())
        }
        (Some(t), false) => TokenAuth::required(t)?,
        (None, true) if loopback => TokenAuth::disabled(),
        (None, true) => {
            return Err("--allow-unauthenticated solo se permite escuchando en loopback".into())
        }
        (None, false) => {
            return Err(
                "falta el token del API: defina AETHERLYNX_API_TOKEN (o use \
                 --allow-unauthenticated en loopback para desarrollo)"
                    .into(),
            )
        }
    };
    if !loopback && a.tls_cert.is_none() && !a.allow_insecure_transport {
        return Err(format!(
            "escuchar en {} sin TLS expondría el token en claro: configure \
             --tls-cert/--tls-key o use --allow-insecure-transport",
            a.listen
        ));
    }
    Ok(auth)
}

async fn shutdown_signal(tx: watch::Sender<bool>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("señal de apagado recibida");
    let _ = tx.send(true);
}

async fn start(a: StartArgs) -> Result<(), String> {
    let auth = security_config(&a)?;
    if !auth.is_enabled() {
        tracing::warn!("autenticación DESACTIVADA: modo desarrollo, solo loopback");
    }

    let registry = Arc::new(Registry::new());
    let runtime = Arc::new(SimulatedRuntime::new(a.scale_step));
    let reconciler = Reconciler::new(registry.clone(), runtime);
    let stats = reconciler.stats.clone();

    let (tx, rx) = watch::channel(false);

    let probe = match &a.integration_url {
        Some(url) => Some(HealthProbe::new(url, Duration::from_secs(5))?),
        None => None,
    };
    let health = probe.as_ref().map(|p| p.state());

    let service = AetherLynxService::new(registry, stats, health);
    let mut builder = Server::builder();
    if let (Some(cert), Some(key)) = (&a.tls_cert, &a.tls_key) {
        let cert =
            std::fs::read(cert).map_err(|e| format!("no se pudo leer el certificado: {e}"))?;
        let key = std::fs::read(key).map_err(|e| format!("no se pudo leer la clave TLS: {e}"))?;
        builder = builder
            .tls_config(ServerTlsConfig::new().identity(Identity::from_pem(cert, key)))
            .map_err(|e| format!("configuración TLS inválida: {e}"))?;
    }

    let reconcile_task = tokio::spawn(reconciler.run(a.reconcile_interval, rx.clone()));
    let probe_task = probe.map(|p| tokio::spawn(p.run(a.integration_interval, rx.clone())));
    tokio::spawn(shutdown_signal(tx.clone()));

    tracing::info!(
        listen = %a.listen,
        tls = a.tls_cert.is_some(),
        auth = auth.is_enabled(),
        "servidor gRPC escuchando"
    );
    let mut srv_rx = rx.clone();
    let served = builder
        .add_service(AetherLynxServer::with_interceptor(service, auth))
        .serve_with_shutdown(a.listen, async move {
            let _ = srv_rx.wait_for(|stop| *stop).await;
        })
        .await;

    // Si el servidor cae, se detiene todo y el proceso sale con error,
    // en vez de seguir vivo sin API como hacía la versión anterior.
    let _ = tx.send(true);
    let _ = reconcile_task.await;
    if let Some(t) = probe_task {
        let _ = t.await;
    }
    served.map_err(|e| match std::error::Error::source(&e) {
        Some(src) => format!("el servidor gRPC terminó con error: {src}"),
        None => format!("el servidor gRPC terminó con error: {e}"),
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Start(args) => {
            init_logging(args.log_format);
            match start(args).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!("{e}");
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
