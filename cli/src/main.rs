use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;

mod grpc_client;
use grpc_client::proto::*;
use grpc_client::{connect, describe};

#[derive(Parser)]
#[command(
    name = "aetherlynx-cli",
    version,
    about = "CLI para el orquestador AetherLynx"
)]
struct Cli {
    /// URL del orquestador (http:// o https://)
    #[arg(
        long,
        global = true,
        env = "AETHERLYNX_ENDPOINT",
        default_value = "http://127.0.0.1:50051"
    )]
    endpoint: String,

    /// Token bearer del API
    #[arg(
        long,
        global = true,
        env = "AETHERLYNX_API_TOKEN",
        hide_env_values = true
    )]
    token: Option<String>,

    /// Certificado de la CA para TLS (PEM)
    #[arg(long, global = true, env = "AETHERLYNX_CA_CERT")]
    ca_cert: Option<PathBuf>,

    /// Formato de salida
    #[arg(long, short, global = true, value_enum, default_value = "table")]
    output: Output,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Output {
    Table,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Gestiona agentes LLM
    #[command(subcommand)]
    Agent(AgentCmd),
    /// Consulta el orquestador
    #[command(subcommand)]
    Orchestrator(OrchestratorCmd),
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Crea un agente
    Create {
        /// Nombre del agente (minúsculas, dígitos y '-')
        #[arg(long)]
        name: String,
        /// Modelo a usar, p. ej. gpt-4o
        #[arg(long)]
        model: String,
        /// Réplicas deseadas
        #[arg(long, default_value_t = 1)]
        replicas: u32,
    },
    /// Cambia el número de réplicas
    Scale {
        #[arg(long)]
        name: String,
        #[arg(long)]
        replicas: u32,
    },
    /// Muestra un agente
    Get {
        #[arg(long)]
        name: String,
    },
    /// Lista los agentes
    List,
    /// Elimina un agente
    Delete {
        #[arg(long)]
        name: String,
    },
}

#[derive(Subcommand)]
enum OrchestratorCmd {
    /// Estado del orquestador
    Status,
}

fn phase_label(p: i32) -> &'static str {
    match AgentPhase::try_from(p).unwrap_or(AgentPhase::Unspecified) {
        AgentPhase::Pending => "Pending",
        AgentPhase::Scaling => "Scaling",
        AgentPhase::Ready => "Ready",
        AgentPhase::Terminating => "Terminating",
        AgentPhase::Failed => "Failed",
        AgentPhase::Unspecified => "Unknown",
    }
}

fn agent_json(a: &Agent) -> serde_json::Value {
    json!({
        "name": a.name, "model": a.model,
        "desired_replicas": a.desired_replicas, "ready_replicas": a.ready_replicas,
        "phase": phase_label(a.phase), "generation": a.generation,
        "created_at_unix": a.created_at_unix, "message": a.message,
    })
}

fn print_agents(agents: &[Agent], out: Output) {
    if out == Output::Json {
        let v: Vec<_> = agents.iter().map(agent_json).collect();
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return;
    }
    if agents.is_empty() {
        println!("No hay agentes.");
        return;
    }
    let w = agents
        .iter()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let wm = agents
        .iter()
        .map(|a| a.model.len())
        .max()
        .unwrap_or(5)
        .max(5);
    println!(
        "{:<w$}  {:<wm$}  {:>7}  {:<11}  MENSAJE",
        "NOMBRE", "MODELO", "LISTAS", "FASE"
    );
    for a in agents {
        let ready = format!("{}/{}", a.ready_replicas, a.desired_replicas);
        println!(
            "{:<w$}  {:<wm$}  {:>7}  {:<11}  {}",
            a.name,
            a.model,
            ready,
            phase_label(a.phase),
            a.message
        );
    }
}

fn print_status(s: &OrchestratorStatus, out: Output) {
    if out == Output::Json {
        let v = json!({
            "version": s.version, "uptime_seconds": s.uptime_seconds, "agents": s.agents,
            "desired_replicas_total": s.desired_replicas_total, "ready_replicas_total": s.ready_replicas_total,
            "reconcile_cycles": s.reconcile_cycles, "last_reconcile_unix": s.last_reconcile_unix,
            "integration": { "enabled": s.integration_enabled, "healthy": s.integration_healthy, "detail": s.integration_detail },
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return;
    }
    println!("Versión:        {}", s.version);
    println!("En marcha:      {} s", s.uptime_seconds);
    println!("Agentes:        {}", s.agents);
    println!(
        "Réplicas:       {}/{} listas",
        s.ready_replicas_total, s.desired_replicas_total
    );
    println!("Ciclos:         {}", s.reconcile_cycles);
    let integ = if !s.integration_enabled {
        "desactivada".to_string()
    } else if s.integration_healthy {
        format!("disponible ({})", s.integration_detail)
    } else {
        format!("NO disponible ({})", s.integration_detail)
    };
    println!("Integración:    {integ}");
}

async fn run(cli: Cli) -> Result<(), String> {
    let mut c = connect(&cli.endpoint, cli.token.as_deref(), cli.ca_cert.as_deref()).await?;
    let out = cli.output;
    let e = |s: tonic::Status| describe(&s);
    match cli.command {
        Command::Agent(AgentCmd::Create {
            name,
            model,
            replicas,
        }) => {
            let a = c
                .create_agent(CreateAgentRequest {
                    name,
                    model,
                    replicas,
                })
                .await
                .map_err(e)?
                .into_inner();
            print_agents(&[a], out);
        }
        Command::Agent(AgentCmd::Scale { name, replicas }) => {
            let a = c
                .scale_agent(ScaleAgentRequest { name, replicas })
                .await
                .map_err(e)?
                .into_inner();
            print_agents(&[a], out);
        }
        Command::Agent(AgentCmd::Get { name }) => {
            let a = c
                .get_agent(GetAgentRequest { name })
                .await
                .map_err(e)?
                .into_inner();
            print_agents(&[a], out);
        }
        Command::Agent(AgentCmd::List) => {
            let l = c
                .list_agents(ListAgentsRequest {})
                .await
                .map_err(e)?
                .into_inner();
            print_agents(&l.agents, out);
        }
        Command::Agent(AgentCmd::Delete { name }) => {
            let a = c
                .delete_agent(DeleteAgentRequest { name })
                .await
                .map_err(e)?
                .into_inner();
            print_agents(&[a], out);
        }
        Command::Orchestrator(OrchestratorCmd::Status) => {
            let s = c
                .get_status(GetStatusRequest {})
                .await
                .map_err(e)?
                .into_inner();
            print_status(&s, out);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}
