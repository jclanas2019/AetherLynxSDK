use tonic::{Request, Response, Status};
use tonic::transport::Error as TonicError;

pub mod proto {
    tonic::include_proto!("aetherlynx");
}

use proto::aether_lynx_client::AetherLynxClient;
use proto::{AgentCreateRequest, OrchestratorStartRequest, AgentCreateResponse, OrchestratorStartResponse};

pub struct AetherLynxGrpcClient;

impl AetherLynxGrpcClient {
    async fn connect() -> Result<AetherLynxClient<tonic::transport::Channel>, TonicError> {
        AetherLynxClient::connect("http://[::1]:50051").await
    }

    pub async fn create_agent(agent_name: &str, model: &str) -> Result<Response<AgentCreateResponse>, String> {
        match Self::connect().await {
            Ok(mut client) => {
                let request = Request::new(AgentCreateRequest {
                    name: agent_name.to_string(),
                    model: model.to_string(),
                });
                client.create_agent(request).await
                    .map_err(|e| format!("Error creating agent: {}", e))
            },
            Err(e) => Err(format!("Failed to connect to the server: {}. Is the server running?", e)),
        }
    }

    pub async fn start_orchestrator() -> Result<Response<OrchestratorStartResponse>, String> {
        match Self::connect().await {
            Ok(mut client) => {
                let request = Request::new(OrchestratorStartRequest {});
                client.start_orchestrator(request).await
                    .map_err(|e| format!("Error starting orchestrator: {}", e))
            },
            Err(e) => Err(format!("Failed to connect to the server: {}. Is the server running?", e)),
        }
    }
}