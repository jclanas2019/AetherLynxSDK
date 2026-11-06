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
use aetherlynx_orchestrator::kubernetes::{AgentTemplates, KubeRuntimeConfig, KubernetesRuntime};
use aetherlynx_orchestrator::proto::aether_lynx_server::AetherLynxServer;
use aetherlynx_orchestrator::reconciler::Reconciler;
use aetherlynx_orchestrator::registry::Registry;
use aetherlynx_orchestrator::runtime::{DesiredState, Runtime, SimulatedRuntime};
use aetherlynx_orchestrator::service::AetherLynxService;
use aetherlynx_orchestrator::telemetry;

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
    /// Muestra el Deployment y el Service que el runtime de Kubernetes
    /// crearía para un agente (JSON, apto para `kubectl diff -f -`)
    RenderAgent(RenderArgs),
}

#[derive(Args)]
struct RenderArgs {
    #[arg(long)]
    name: String,
    #[arg(long)]
    model: String,
    #[arg(long, default_value_t = 1)]
    replicas: u32,
    /// Namespace de los objetos
    #[arg(long, default_value = "aetherlynx")]
    namespace: String,
    #[command(flatten)]
    k8s: K8sArgs,
}

#[derive(Clone, Copy, ValueEnum)]
enum RuntimeKind {
    /// Réplicas en memoria, para desarrollo y pruebas
    Simulated,
    /// Un Deployment y un Service por agente en Kubernetes
    Kubernetes,
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

    /// Dónde viven las réplicas de los agentes
    #[arg(
        long,
        env = "AETHERLYNX_RUNTIME",
        value_enum,
        default_value = "simulated"
    )]
    runtime: RuntimeKind,

    /// Réplicas que el runtime simulado sube o baja por ciclo
    #[arg(long, default_value_t = 1)]
    scale_step: u32,

    #[command(flatten)]
    k8s: K8sArgs,

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

/// Opciones del runtime de Kubernetes (solo con `--runtime kubernetes`).
#[derive(Args)]
#[command(next_help_heading = "Runtime de Kubernetes")]
struct K8sArgs {
    /// Imagen del contenedor de los agentes (obligatoria). Fije una versión.
    #[arg(long, env = "AETHERLYNX_AGENT_IMAGE")]
    agent_image: Option<String>,

    /// Namespace de los agentes (por defecto, el del orquestador o el del kubeconfig)
    #[arg(long, env = "AETHERLYNX_K8S_NAMESPACE")]
    k8s_namespace: Option<String>,

    /// Distingue a varios orquestadores en un mismo namespace
    #[arg(long, env = "AETHERLYNX_INSTANCE", default_value = "default")]
    instance: String,

    /// Secret con las credenciales de los agentes ("" para ninguno)
    #[arg(
        long,
        env = "AETHERLYNX_AGENT_SECRET",
        default_value = "university-agent-secrets"
    )]
    agent_secret: String,

    /// ConfigMap con la configuración de los agentes ("" para ninguno)
    #[arg(
        long,
        env = "AETHERLYNX_AGENT_CONFIGMAP",
        default_value = "university-agent-config"
    )]
    agent_configmap: String,

    /// ServiceAccount de los pods de agente ("" para la del namespace)
    #[arg(
        long,
        env = "AETHERLYNX_AGENT_SERVICE_ACCOUNT",
        default_value = "university-agent"
    )]
    agent_service_account: String,

    /// Prefijo de los objetos de telemetría de los agentes: ConfigMap
    /// `<p>-client`, Secret `<p>-ingest` y ConfigMap `<p>-ca`, todos
    /// opcionales ("" para no usarlos)
    #[arg(
        long,
        env = "AETHERLYNX_AGENT_TELEMETRY",
        default_value = "aetherlynx-telemetry"
    )]
    agent_telemetry: String,

    #[arg(long, default_value = "100m")]
    agent_cpu_request: String,
    #[arg(long, default_value = "192Mi")]
    agent_memory_request: String,
    #[arg(long, default_value = "512Mi")]
    agent_memory_limit: String,
}

fn non_empty(s: &str) -> Option<String> {
    (!s.trim().is_empty()).then(|| s.trim().to_owned())
}

impl K8sArgs {
    fn config(&self) -> Result<KubeRuntimeConfig, String> {
        let image = self.agent_image.clone().ok_or(
            "con --runtime kubernetes hay que indicar --agent-image (AETHERLYNX_AGENT_IMAGE)",
        )?;
        Ok(KubeRuntimeConfig {
            namespace: self.k8s_namespace.clone(),
            image,
            instance: self.instance.clone(),
            env_from_secret: non_empty(&self.agent_secret),
            env_from_configmap: non_empty(&self.agent_configmap),
            service_account: non_empty(&self.agent_service_account),
            telemetry: non_empty(&self.agent_telemetry),
            container_port: 5000,
            cpu_request: self.agent_cpu_request.clone(),
            memory_request: self.agent_memory_request.clone(),
            memory_limit: self.agent_memory_limit.clone(),
        })
    }
}

fn render_agent(a: RenderArgs) -> Result<(), String> {
    use aetherlynx_orchestrator::validation;
    validation::agent_name(&a.name).map_err(|e| e.to_string())?;
    validation::model_id(&a.model).map_err(|e| e.to_string())?;
    validation::replicas(a.replicas).map_err(|e| e.to_string())?;
    let t = AgentTemplates::new(a.k8s.config()?, a.namespace)?;
    let d = DesiredState {
        name: &a.name,
        model: &a.model,
        desired_replicas: a.replicas,
        deleting: false,
    };
    let to_json = |v: serde_json::Result<serde_json::Value>| v.map_err(|e| e.to_string());
    let mut dep = to_json(serde_json::to_value(t.deployment_manifest(&d)))?;
    let mut svc = to_json(serde_json::to_value(t.service_manifest(&d, None)))?;
    dep["apiVersion"] = "apps/v1".into();
    dep["kind"] = "Deployment".into();
    svc["apiVersion"] = "v1".into();
    svc["kind"] = "Service".into();
    let list = serde_json::json!({"apiVersion": "v1", "kind": "List", "items": [dep, svc]});
    println!(
        "{}",
        serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?
    );
    Ok(())
}

async fn build_runtime(a: &StartArgs) -> Result<Arc<dyn Runtime>, String> {
    match a.runtime {
        RuntimeKind::Simulated => Ok(Arc::new(SimulatedRuntime::new(a.scale_step))),
        RuntimeKind::Kubernetes => {
            let k = &a.k8s;
            let rt = KubernetesRuntime::connect(k.config()?).await?;
            tracing::info!(namespace = rt.namespace(), instance = %k.instance, "runtime de Kubernetes listo");
            Ok(Arc::new(rt))
        }
    }
}

/// Gauges observables: se leen del registro cada vez que se exportan.
fn register_gauges(
    registry: Arc<Registry>,
    stats: Arc<aetherlynx_orchestrator::reconciler::ReconcileStats>,
) {
    use aetherlynx_orchestrator::proto::AgentPhase;
    use aetherlynx_orchestrator::telemetry::{meter, names};
    use opentelemetry::KeyValue;
    let started = aetherlynx_orchestrator::unix_now();
    let m = meter();
    let reg = registry.clone();
    let _ = m
        .u64_observable_gauge(names::AGENTS)
        .with_description("Agentes por fase")
        .with_callback(move |o| {
            let agents = reg.list();
            for (phase, label) in [
                (AgentPhase::Pending, "pending"),
                (AgentPhase::Scaling, "scaling"),
                (AgentPhase::Ready, "ready"),
                (AgentPhase::Terminating, "terminating"),
                (AgentPhase::Failed, "failed"),
            ] {
                let n = agents.iter().filter(|a| a.phase == phase).count() as u64;
                o.observe(n, &[KeyValue::new("phase", label)]);
            }
        })
        .build();
    let reg = registry.clone();
    let _ = m
        .u64_observable_gauge(names::REPLICAS_DESIRED)
        .with_description("Réplicas pedidas, total de todos los agentes")
        .with_callback(move |o| {
            o.observe(
                reg.list()
                    .iter()
                    .map(|a| u64::from(a.desired_replicas))
                    .sum(),
                &[],
            )
        })
        .build();
    let reg = registry;
    let _ = m
        .u64_observable_gauge(names::REPLICAS_READY)
        .with_description("Réplicas listas, total de todos los agentes")
        .with_callback(move |o| {
            o.observe(
                reg.list().iter().map(|a| u64::from(a.ready_replicas)).sum(),
                &[],
            )
        })
        .build();
    let _ = m
        .i64_observable_gauge(names::RECONCILE_LAST_SUCCESS)
        .with_unit("s")
        .with_description("Momento (unix) del último ciclo de reconciliación con progreso")
        .with_callback(move |o| {
            // Hasta el primer éxito se informa la hora de arranque: si nunca
            // llega a haber uno, la alerta salta igual pasado el umbral.
            let t = stats
                .last_success_unix
                .load(std::sync::atomic::Ordering::Relaxed);
            o.observe(if t > 0 { t } else { started }, &[]);
        })
        .build();
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
    let runtime = build_runtime(&a).await?;
    let runtime_kind = runtime.kind();
    let reconciler = Reconciler::new(registry.clone(), runtime);
    let stats = reconciler.stats.clone();
    register_gauges(registry.clone(), stats.clone());

    // Antes de aceptar peticiones: recuperar los agentes que ya existen en el
    // runtime, para no duplicarlos ni borrarlos como huérfanos.
    let recovered = reconciler.recover().await?;
    if recovered > 0 {
        tracing::info!(agents = recovered, "agentes recuperados del runtime");
    }

    let (tx, rx) = watch::channel(false);

    let probe = match &a.integration_url {
        Some(url) => Some(HealthProbe::new(url, Duration::from_secs(5))?),
        None => None,
    };
    let health = probe.as_ref().map(|p| p.state());

    let service = AetherLynxService::new(registry, stats, health).with_runtime_kind(runtime_kind);
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
        Command::RenderAgent(args) => match render_agent(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Start(args) => {
            let mut telemetry = match telemetry::init(matches!(args.log_format, LogFormat::Json)) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("error: telemetría: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if let Some(ep) = &telemetry.endpoint {
                tracing::info!(endpoint = %ep, "exportando telemetría por OTLP");
            }
            let code = match start(args).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!("{e}");
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
            // Envía lo pendiente antes de salir. Se hace fuera del runtime
            // async porque los exportadores usan un cliente bloqueante.
            tokio::task::spawn_blocking(move || telemetry.shutdown())
                .await
                .ok();
            code
        }
    }
}
