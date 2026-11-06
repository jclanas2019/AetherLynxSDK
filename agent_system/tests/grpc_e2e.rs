//! Levanta el servidor real en un puerto efímero y lo ejercita con el
//! cliente gRPC generado.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Server};
use tonic::{Code, Request};

use aetherlynx_orchestrator::auth::TokenAuth;
use aetherlynx_orchestrator::proto::aether_lynx_client::AetherLynxClient;
use aetherlynx_orchestrator::proto::aether_lynx_server::AetherLynxServer;
use aetherlynx_orchestrator::proto::*;
use aetherlynx_orchestrator::reconciler::Reconciler;
use aetherlynx_orchestrator::registry::Registry;
use aetherlynx_orchestrator::runtime::SimulatedRuntime;
use aetherlynx_orchestrator::service::AetherLynxService;

const TOKEN: &str = "test-token-0123456789abcdef0123456789";

async fn spawn_server() -> (String, Arc<Registry>, Reconciler) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let registry = Arc::new(Registry::new());
    let reconciler = Reconciler::new(registry.clone(), Arc::new(SimulatedRuntime::new(2)));
    let svc = AetherLynxService::new(registry.clone(), reconciler.stats.clone(), None);
    let auth = TokenAuth::required(TOKEN).unwrap();
    tokio::spawn(
        Server::builder()
            .add_service(AetherLynxServer::with_interceptor(svc, auth))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    (format!("http://{addr}"), registry, reconciler)
}

async fn client(url: &str) -> AetherLynxClient<Channel> {
    for _ in 0..50 {
        if let Ok(c) = AetherLynxClient::connect(url.to_owned()).await {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("el servidor no arrancó");
}

fn authed<T>(msg: T) -> Request<T> {
    let mut r = Request::new(msg);
    r.metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    r
}

#[tokio::test]
async fn full_lifecycle() {
    let (url, _reg, reconciler) = spawn_server().await;
    let mut c = client(&url).await;

    let a = c
        .create_agent(authed(CreateAgentRequest {
            name: "student-agent".into(),
            model: "gpt-4o".into(),
            replicas: 3,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(a.phase(), AgentPhase::Pending);

    c.scale_agent(authed(ScaleAgentRequest {
        name: "student-agent".into(),
        replicas: 5,
    }))
    .await
    .unwrap();
    for _ in 0..3 {
        reconciler.reconcile_once();
    }
    let a = c
        .get_agent(authed(GetAgentRequest {
            name: "student-agent".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((a.ready_replicas, a.phase()), (5, AgentPhase::Ready));

    let st = c
        .get_status(authed(GetStatusRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((st.agents, st.ready_replicas_total), (1, 5));

    c.delete_agent(authed(DeleteAgentRequest {
        name: "student-agent".into(),
    }))
    .await
    .unwrap();
    for _ in 0..3 {
        reconciler.reconcile_once();
    }
    let l = c
        .list_agents(authed(ListAgentsRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(l.agents.is_empty());
}

#[tokio::test]
async fn rejects_missing_or_wrong_token() {
    let (url, reg, _r) = spawn_server().await;
    let mut c = client(&url).await;
    let err = c
        .list_agents(Request::new(ListAgentsRequest {}))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);

    let mut r = Request::new(CreateAgentRequest {
        name: "x".into(),
        model: "m".into(),
        replicas: 1,
    });
    r.metadata_mut()
        .insert("authorization", "Bearer nope".parse().unwrap());
    assert_eq!(
        c.create_agent(r).await.unwrap_err().code(),
        Code::Unauthenticated
    );
    assert!(
        reg.list().is_empty(),
        "una petición sin auth no debe tocar el estado"
    );
}

#[tokio::test]
async fn invalid_input_maps_to_invalid_argument() {
    let (url, _reg, _r) = spawn_server().await;
    let mut c = client(&url).await;
    let err = c
        .create_agent(authed(CreateAgentRequest {
            name: "x\nFAKE LOG".into(),
            model: "m".into(),
            replicas: 1,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);

    let err = c
        .scale_agent(authed(ScaleAgentRequest {
            name: "missing".into(),
            replicas: 1,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}
