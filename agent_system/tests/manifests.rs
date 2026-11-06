//! Los manifiestos de `kubernetes/` se deserializan con los tipos de
//! Kubernetes (detecta errores de forma) y mantienen la coherencia con lo
//! que asume el código: nombres de objetos de telemetría y etiquetas.

use std::path::{Path, PathBuf};

use aetherlynx_orchestrator::kubernetes::{KubeRuntimeConfig, TELEMETRY_CA_DIR};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, PodSpec, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use serde_json::Value;

fn manifests(dir: &str) -> Vec<(PathBuf, Value)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(dir);
    let mut out = Vec::new();
    for e in std::fs::read_dir(&root).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "yaml") {
            let text = std::fs::read_to_string(&p).unwrap();
            for doc in text.split("\n---\n") {
                let v: Value = serde_yaml::from_str(doc)
                    .unwrap_or_else(|e| panic!("{}: YAML inválido: {e}", p.display()));
                if !v.is_null() {
                    out.push((p.clone(), v));
                }
            }
        }
    }
    out
}

fn deployment(name: &str) -> Deployment {
    let v = manifests("kubernetes")
        .into_iter()
        .map(|(_, v)| v)
        .find(|v| v["kind"] == "Deployment" && v["metadata"]["name"] == name)
        .unwrap_or_else(|| panic!("falta el Deployment {name}"));
    serde_json::from_value(v).unwrap()
}

#[test]
fn every_manifest_has_a_valid_shape() {
    for dir in ["kubernetes", "kubernetes/examples"] {
        for (path, v) in manifests(dir) {
            let kind = v["kind"].as_str().unwrap_or_default().to_owned();
            let r = match kind.as_str() {
                "Deployment" => serde_json::from_value::<Deployment>(v).map(drop),
                "Service" => serde_json::from_value::<Service>(v).map(drop),
                "ConfigMap" => serde_json::from_value::<ConfigMap>(v).map(drop),
                "Secret" => serde_json::from_value::<Secret>(v).map(drop),
                "NetworkPolicy" => serde_json::from_value::<NetworkPolicy>(v).map(drop),
                _ => Ok(()),
            };
            r.unwrap_or_else(|e| panic!("{}: {kind}: {e}", path.display()));
        }
    }
}

fn pod(d: &Deployment) -> &PodSpec {
    d.spec.as_ref().unwrap().template.spec.as_ref().unwrap()
}

#[test]
fn telemetry_clients_use_the_names_the_runtime_uses() {
    let prefix = KubeRuntimeConfig::new("x:1").telemetry.unwrap();
    for name in ["aetherlynx-orchestrator", "university-agent"] {
        let d = deployment(name);
        let p = pod(&d);
        let c = &p.containers[0];
        let env_from = c.env_from.as_ref().unwrap();
        let cm = env_from
            .iter()
            .filter_map(|e| e.config_map_ref.as_ref())
            .find(|r| r.name == format!("{prefix}-client"))
            .unwrap_or_else(|| panic!("{name}: sin ConfigMap de telemetría"));
        assert_eq!(cm.optional, Some(true), "{name}: debe ser opcional");
        let hdr = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "OTEL_EXPORTER_OTLP_HEADERS")
            .unwrap();
        let sk = hdr
            .value_from
            .as_ref()
            .unwrap()
            .secret_key_ref
            .as_ref()
            .unwrap();
        assert_eq!(sk.name, format!("{prefix}-ingest"));
        assert_eq!(sk.optional, Some(true));
        let vol = p
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "telemetry-ca")
            .unwrap();
        assert_eq!(
            vol.config_map.as_ref().unwrap().name,
            format!("{prefix}-ca")
        );
        assert!(c
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|m| m.name == "telemetry-ca" && m.mount_path == TELEMETRY_CA_DIR));
    }

    // Lo que el colector publica para los clientes apunta a ese directorio.
    let client = manifests("kubernetes")
        .into_iter()
        .map(|(_, v)| v)
        .find(|v| v["kind"] == "ConfigMap" && v["metadata"]["name"] == format!("{prefix}-client"))
        .unwrap();
    assert_eq!(
        client["data"]["OTEL_EXPORTER_OTLP_CERTIFICATE"],
        format!("{TELEMETRY_CA_DIR}/ca.crt")
    );
    assert!(client["data"]["OTEL_EXPORTER_OTLP_ENDPOINT"]
        .as_str()
        .unwrap()
        .starts_with("https://"));
}

#[test]
fn the_collector_is_hardened_and_isolated() {
    let d = deployment("aetherlynx-telemetry");
    assert_eq!(d.spec.as_ref().unwrap().replicas, Some(1));
    let p = pod(&d);
    assert_eq!(p.automount_service_account_token, Some(false));
    let sc = p.containers[0].security_context.as_ref().unwrap();
    assert_eq!(sc.read_only_root_filesystem, Some(true));
    assert_eq!(sc.allow_privilege_escalation, Some(false));

    let np: NetworkPolicy = serde_json::from_value(
        manifests("kubernetes")
            .into_iter()
            .map(|(_, v)| v)
            .find(|v| {
                v["kind"] == "NetworkPolicy" && v["metadata"]["name"] == "aetherlynx-telemetry"
            })
            .unwrap(),
    )
    .unwrap();
    let spec = np.spec.unwrap();
    assert_eq!(spec.egress, Some(vec![]), "el colector no necesita salida");

    // Agentes y orquestador pueden llegar al colector a pesar de sus
    // políticas de salida restrictivas.
    for policy in ["university-agent", "aetherlynx-orchestrator"] {
        let v = manifests("kubernetes")
            .into_iter()
            .map(|(_, v)| v)
            .find(|v| v["kind"] == "NetworkPolicy" && v["metadata"]["name"] == policy)
            .unwrap();
        let egress = v["spec"]["egress"].as_array().unwrap();
        assert!(
            egress.iter().any(|r| {
                r["to"][0]["podSelector"]["matchLabels"]["app.kubernetes.io/name"]
                    == "aetherlynx-telemetry"
                    && r["ports"][0]["port"] == 4318
            }),
            "{policy}: sin salida hacia el colector"
        );
    }
}
