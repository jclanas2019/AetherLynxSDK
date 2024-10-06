use clap::{Arg, Command};
use crate::grpc_client::AetherLynxGrpcClient;

mod grpc_client;

fn print_help() {
    println!("AetherLynx CLI - Command Line Interface for AetherLynx SDK

Usage:
  aetherlynx-cli [COMMAND]

Commands:
  agent         Manage LLM agents
    create      Create a new LLM agent
    scale       Scale an agent
    list        List all running agents
    delete      Delete an agent
  orchestrator  Manage the orchestrator
    start       Start the orchestrator
    stop        Stop the orchestrator
    status      Check the status of the orchestrator
    set-desired-state  Set the desired state of agents
  metrics       View system metrics
  logs          View system logs
  help          Show this help message

For more information about a specific command, run:
  aetherlynx-cli [COMMAND] --help");
}

#[tokio::main]
async fn main() {
    let matches = Command::new("AetherLynx CLI")
        .version("1.0")
        .about("Command Line Interface for AetherLynx SDK")
        .subcommand(
            Command::new("agent")
                .about("Manage LLM agents")
                .subcommand(
                    Command::new("create")
                        .about("Create a new LLM agent")
                        .arg(Arg::new("agent-name").required(true).help("Name of the agent"))
                        .arg(Arg::new("model").long("model").required(true).help("Model to use")),
                )
                .subcommand(
                    Command::new("scale")
                        .about("Scale an agent")
                        .arg(Arg::new("agent-name").required(true).help("Name of the agent"))
                        .arg(Arg::new("replicas").long("replicas").required(true).help("Number of replicas")),
                )
                .subcommand(Command::new("list").about("List all running agents"))
                .subcommand(
                    Command::new("delete")
                        .about("Delete an agent")
                        .arg(Arg::new("agent-name").required(true).help("Name of the agent")),
                ),
        )
        .subcommand(
            Command::new("orchestrator")
                .about("Manage the orchestrator")
                .subcommand(Command::new("start").about("Start the orchestrator"))
                .subcommand(Command::new("stop").about("Stop the orchestrator"))
                .subcommand(Command::new("status").about("Check the status of the orchestrator"))
                .subcommand(
                    Command::new("set-desired-state")
                        .about("Set the desired state of agents")
                        .arg(Arg::new("agents").long("agents").required(true).help("Number of agents")),
                ),
        )
        .subcommand(Command::new("metrics").about("View system metrics"))
        .subcommand(Command::new("logs").about("View system logs"))
        .subcommand(Command::new("help").about("Show help information"))
        .get_matches();

    if let Some((subcommand, sub_matches)) = matches.subcommand() {
        match subcommand {
            "agent" => {
                if let Some(create_matches) = sub_matches.subcommand_matches("create") {
                    let agent_name = create_matches.get_one::<String>("agent-name").unwrap();
                    let model = create_matches.get_one::<String>("model").unwrap();
                    println!("Creating agent {} with model {}", agent_name, model);
                    match AetherLynxGrpcClient::create_agent(agent_name, model).await {
                        Ok(response) => println!("Agent created successfully: {:?}", response),
                        Err(e) => eprintln!("Error creating agent: {:?}", e),
                    }
                }
            }
            "orchestrator" => {
                if sub_matches.subcommand_matches("start").is_some() {
                    println!("Starting orchestrator...");
                    match AetherLynxGrpcClient::start_orchestrator().await {
                        Ok(response) => println!("Orchestrator started successfully: {:?}", response),
                        Err(e) => eprintln!("Error starting orchestrator: {:?}", e),
                    }
                }
            }
            "help" => print_help(),
            _ => println!("Invalid command. Use 'aetherlynx-cli help' for usage information."),
        }
    } else {
        print_help();
    }
}