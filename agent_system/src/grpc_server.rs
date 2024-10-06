use tonic::{transport::Server, Request, Response, Status};

pub mod aetherlynx {
    tonic::include_proto!("aetherlynx");
}

use aetherlynx::aether_lynx_server::{AetherLynx, AetherLynxServer};
use aetherlynx::{AgentCreateRequest, AgentCreateResponse, OrchestratorStartRequest, OrchestratorStartResponse};

#[derive(Default)]
pub struct AetherLynxService;

#[tonic::async_trait]
impl AetherLynx for AetherLynxService {
    async fn create_agent(
        &self,
        request: Request<AgentCreateRequest>,
    ) -> Result<Response<AgentCreateResponse>, Status> {
        let agent = request.into_inner();
        println!("Creating agent: {} with model: {}", agent.name, agent.model);
        Ok(Response::new(AgentCreateResponse {}))
    }

    async fn start_orchestrator(
        &self,
        _request: Request<OrchestratorStartRequest>,
    ) -> Result<Response<OrchestratorStartResponse>, Status> {
        println!("Starting orchestrator");
        Ok(Response::new(OrchestratorStartResponse {}))
    }
}

pub async fn run_grpc_server() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50051".parse()?;
    let service = AetherLynxService::default();

    Server::builder()
        .add_service(AetherLynxServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}