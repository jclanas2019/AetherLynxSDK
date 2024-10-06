use clap::Command;
mod grpc_server;
mod controllers;
mod integration;

fn print_help() {
    println!("AetherLynx Agent System

Usage:
  agent_system [COMMAND]

Commands:
  start     Inicia el sistema de agentes (servidor gRPC y controlador)
  help      Muestra este mensaje de ayuda

Para más información sobre un comando específico, ejecute:
  agent_system [COMMAND] --help");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = Command::new("AetherLynx Agent System")
        .version("1.0")
        .about("Sistema de agentes para AetherLynx")
        .subcommand(Command::new("start").about("Inicia el sistema de agentes"))
        .subcommand(Command::new("help").about("Muestra información de ayuda"))
        .get_matches();

    match matches.subcommand() {
        Some(("start", _)) => {
            // Inicializa el servidor gRPC
            let grpc_server_handle = tokio::spawn(async {
                if let Err(e) = grpc_server::run_grpc_server().await {
                    eprintln!("Error en el servidor gRPC: {}", e);
                }
            });

            // Inicializa el controlador de reconciliación
            let university_controller = controllers::UniversityController {};
            let controller_handle = tokio::spawn(async move {
                university_controller.reconcile().await;
            });

            // Espera a que ambos manejos terminen
            tokio::try_join!(grpc_server_handle, controller_handle)?;
        }
        Some(("help", _)) | None => {
            print_help();
        }
        _ => {
            println!("Comando no reconocido. Use 'agent_system help' para ver los comandos disponibles.");
        }
    }

    Ok(())
}