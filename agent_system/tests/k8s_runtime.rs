//! Tests del runtime de Kubernetes contra un API server falso a nivel HTTP.
//!
//! El cliente `kube` real envía peticiones HTTP reales a este servicio, que
//! emula el subconjunto del API que usa el runtime: server-side apply (con
//! generaciones), GET, LIST con selector de etiquetas y DELETE (inmediato o
//! "lento", como cuando los pods tardan en terminar). Así se verifica qué
//! rutas, verbos, parámetros y cuerpos envía el runtime, y cómo interpreta
//! las respuestas.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use kube::Client;
use serde_json::{json, Value};

use aetherlynx_orchestrator::kubernetes::{KubeRuntimeConfig, KubernetesRuntime};
use aetherlynx_orchestrator::proto::AgentPhase;
use aetherlynx_orchestrator::reconciler::Reconciler;
use aetherlynx_orchestrator::registry::Registry;
use aetherlynx_orchestrator::runtime::{DesiredState, Observation, Runtime};

const NS: &str = "aetherlynx";

#[derive(Debug, Clone)]
struct Call {
    method: String,
    path: String,
    query: String,
    body: Value,
}

#[derive(Default)]
struct FakeState {
    /// (recurso, nombre) -> objeto
    objects: BTreeMap<(String, String), Value>,
    calls: Vec<Call>,
    next_uid: u64,
    /// Si es true, DELETE deja el objeto con deletionTimestamp (pods terminando).
    slow_delete: bool,
    /// Si es true, todas las escrituras responden 403.
    forbid: bool,
    /// Cuántos DELETE siguientes fallan con 500 (borrado interrumpido).
    fail_deletes: u32,
}

#[derive(Clone, Default)]
struct FakeApi(Arc<Mutex<FakeState>>);

fn status_body(code: u16, reason: &str, msg: &str) -> Value {
    json!({"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure",
           "message": msg, "reason": reason, "code": code})
}

fn label_match(obj: &Value, selector: &str) -> bool {
    selector.split(',').filter(|s| !s.is_empty()).all(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        obj["metadata"]["labels"][k].as_str() == Some(v)
    })
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| {
            v.replace("%2C", ",")
                .replace("%3D", "=")
                .replace("%2F", "/")
        })
    })
}

impl FakeApi {
    fn client(&self) -> Client {
        let me = self.clone();
        let svc = tower::service_fn(move |req: Request<kube::client::Body>| {
            let me = me.clone();
            async move { Ok::<_, Infallible>(me.handle(req).await) }
        });
        Client::new(svc, NS)
    }

    fn runtime(&self) -> KubernetesRuntime {
        KubernetesRuntime::with_client(
            self.client(),
            KubeRuntimeConfig::new("registry.example.com/agent:0.2.0"),
        )
        .unwrap()
    }

    fn calls(&self) -> Vec<Call> {
        self.0.lock().unwrap().calls.clone()
    }

    fn get(&self, kind: &str, name: &str) -> Option<Value> {
        self.0
            .lock()
            .unwrap()
            .objects
            .get(&(kind.into(), name.into()))
            .cloned()
    }

    fn insert(&self, kind: &str, obj: Value) {
        let name = obj["metadata"]["name"].as_str().unwrap().to_owned();
        self.0
            .lock()
            .unwrap()
            .objects
            .insert((kind.into(), name), obj);
    }

    /// Simula al controlador de Deployments informando el estado.
    fn set_status(&self, name: &str, status: Value) {
        let mut st = self.0.lock().unwrap();
        let obj = st
            .objects
            .get_mut(&("deployments".into(), name.into()))
            .unwrap();
        obj["status"] = status;
    }

    /// Estado de un rollout terminado con `n` réplicas.
    fn finish_rollout(&self, name: &str, n: i64) {
        let generation = self.get("deployments", name).unwrap()["metadata"]["generation"].clone();
        self.set_status(
            name,
            json!({"observedGeneration": generation, "replicas": n, "updatedReplicas": n, "readyReplicas": n}),
        );
    }

    /// Simula al recolector de basura terminando un borrado lento.
    fn finalize_deletes(&self) {
        let mut st = self.0.lock().unwrap();
        st.objects
            .retain(|_, o| o["metadata"]["deletionTimestamp"].is_null());
    }

    async fn handle(&self, req: Request<kube::client::Body>) -> Response<Full<Bytes>> {
        let (parts, body) = req.into_parts();
        let raw = body
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        let method = parts.method.as_str().to_owned();
        let content_type = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let path = parts.uri.path().to_owned();
        let query = parts.uri.query().unwrap_or("").to_owned();
        let mut st = self.0.lock().unwrap();
        st.calls.push(Call {
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            body: body.clone(),
        });

        let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        // /apis/apps/v1/namespaces/{ns}/deployments[/{name}]  |  /api/v1/namespaces/{ns}/services[/{name}]
        let Some(i) = segs
            .iter()
            .position(|s| *s == "deployments" || *s == "services")
        else {
            return reply(
                StatusCode::NOT_FOUND,
                status_body(404, "NotFound", "ruta desconocida"),
            );
        };
        assert_eq!(segs[i - 1], NS, "namespace inesperado en {path}");
        let kind = segs[i].to_owned();
        let name = segs.get(i + 1).map(|s| s.to_string());

        if st.forbid && method != "GET" {
            return reply(
                StatusCode::FORBIDDEN,
                status_body(
                    403,
                    "Forbidden",
                    "deployments is forbidden: User cannot patch",
                ),
            );
        }

        match (method.as_str(), name) {
            // JSON merge patch: solo se usa para las anotaciones.
            ("PATCH", Some(name)) if content_type.contains("merge-patch") => {
                let Some(obj) = st.objects.get_mut(&(kind, name.clone())) else {
                    return reply(
                        StatusCode::NOT_FOUND,
                        status_body(404, "NotFound", "not found"),
                    );
                };
                if let Some(ann) = body["metadata"]["annotations"].as_object() {
                    if !obj["metadata"]["annotations"].is_object() {
                        obj["metadata"]["annotations"] = json!({});
                    }
                    for (k, v) in ann {
                        if v.is_null() {
                            obj["metadata"]["annotations"]
                                .as_object_mut()
                                .unwrap()
                                .remove(k);
                        } else {
                            obj["metadata"]["annotations"][k] = v.clone();
                        }
                    }
                }
                reply(StatusCode::OK, obj.clone())
            }
            // Server-side apply.
            ("PATCH", Some(name)) => {
                assert!(
                    content_type.contains("apply-patch"),
                    "PATCH inesperado: {content_type}"
                );
                let key = (kind.clone(), name.clone());
                st.next_uid += 1;
                let new_uid = format!("uid-{}", st.next_uid);
                let existed = st.objects.contains_key(&key);
                let mut obj = st.objects.get(&key).cloned().unwrap_or_else(|| {
                    json!({"metadata": {"name": name, "namespace": NS, "uid": new_uid, "generation": 0}})
                });
                // Como el API real: el selector de un Deployment es inmutable.
                if kind == "deployments"
                    && existed
                    && obj["spec"]["selector"] != body["spec"]["selector"]
                {
                    return reply(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        status_body(
                            422,
                            "Invalid",
                            "spec.selector: Invalid value: field is immutable",
                        ),
                    );
                }
                let spec_changed = obj["spec"] != body["spec"];
                // Etiquetas y anotaciones se fusionan (otros gestores conservan las suyas).
                for field in ["labels", "annotations"] {
                    if let Some(m) = body["metadata"][field].as_object() {
                        if !obj["metadata"][field].is_object() {
                            obj["metadata"][field] = json!({});
                        }
                        for (k, v) in m {
                            obj["metadata"][field][k] = v.clone();
                        }
                    }
                }
                if !body["metadata"]["ownerReferences"].is_null() {
                    obj["metadata"]["ownerReferences"] =
                        body["metadata"]["ownerReferences"].clone();
                }
                obj["spec"] = body["spec"].clone();
                obj["apiVersion"] = body["apiVersion"].clone();
                obj["kind"] = body["kind"].clone();
                if spec_changed {
                    let g = obj["metadata"]["generation"].as_i64().unwrap_or(0) + 1;
                    obj["metadata"]["generation"] = json!(g);
                }
                st.objects.insert(key, obj.clone());
                reply(StatusCode::OK, obj)
            }
            ("GET", Some(name)) => match st.objects.get(&(kind, name.clone())) {
                Some(o) => reply(StatusCode::OK, o.clone()),
                None => reply(
                    StatusCode::NOT_FOUND,
                    status_body(404, "NotFound", &format!("\"{name}\" not found")),
                ),
            },
            ("GET", None) => {
                let sel = query_param(&query, "labelSelector").unwrap_or_default();
                let items: Vec<Value> = st
                    .objects
                    .iter()
                    .filter(|((k, _), o)| *k == kind && label_match(o, &sel))
                    .map(|(_, o)| o.clone())
                    .collect();
                let list_kind = if kind == "deployments" {
                    "DeploymentList"
                } else {
                    "ServiceList"
                };
                reply(
                    StatusCode::OK,
                    json!({"apiVersion": "v1", "kind": list_kind, "metadata": {"resourceVersion": "1"}, "items": items}),
                )
            }
            ("DELETE", Some(name)) => {
                if st.fail_deletes > 0 {
                    st.fail_deletes -= 1;
                    return reply(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        status_body(500, "InternalError", "etcd timeout"),
                    );
                }
                let key = (kind, name.clone());
                let slow = st.slow_delete;
                let Some(current) = st.objects.get(&key).cloned() else {
                    return reply(
                        StatusCode::NOT_FOUND,
                        status_body(404, "NotFound", &format!("\"{name}\" not found")),
                    );
                };
                if let Some(uid) = body["preconditions"]["uid"].as_str() {
                    if current["metadata"]["uid"].as_str() != Some(uid) {
                        return reply(
                            StatusCode::CONFLICT,
                            status_body(
                                409,
                                "Conflict",
                                "Precondition failed: UID in precondition",
                            ),
                        );
                    }
                }
                if slow {
                    let o = st.objects.get_mut(&key).unwrap();
                    o["metadata"]["deletionTimestamp"] = json!("2026-11-06T00:00:00Z");
                    reply(StatusCode::OK, o.clone())
                } else {
                    reply(StatusCode::OK, st.objects.remove(&key).unwrap())
                }
            }
            _ => reply(
                StatusCode::METHOD_NOT_ALLOWED,
                status_body(405, "MethodNotAllowed", "no soportado"),
            ),
        }
    }
}

fn reply(code: StatusCode, v: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(serde_json::to_vec(&v).unwrap())))
        .unwrap()
}

fn want(name: &'static str, n: u32) -> DesiredState<'static> {
    DesiredState {
        name,
        model: "gpt-4o-mini",
        desired_replicas: n,
        deleting: false,
    }
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn apply_uses_server_side_apply_and_creates_an_owned_service() {
    let api = FakeApi::default();
    let rt = api.runtime();
    let obs = rt.apply(&want("student-agent", 3)).await.unwrap();
    assert_eq!(
        obs,
        Observation {
            ready: 0,
            converged: false
        }
    );

    let calls = api.calls();
    let patches: Vec<_> = calls.iter().filter(|c| c.method == "PATCH").collect();
    assert_eq!(patches.len(), 2);
    assert_eq!(
        patches[0].path,
        "/apis/apps/v1/namespaces/aetherlynx/deployments/aetherlynx-student-agent"
    );
    assert_eq!(
        patches[1].path,
        "/api/v1/namespaces/aetherlynx/services/aetherlynx-student-agent"
    );
    for p in &patches {
        assert_eq!(
            query_param(&p.query, "fieldManager").as_deref(),
            Some("aetherlynx-orchestrator")
        );
        assert_eq!(query_param(&p.query, "force").as_deref(), Some("true"));
    }

    let dep = api.get("deployments", "aetherlynx-student-agent").unwrap();
    assert_eq!(dep["spec"]["replicas"], 3);
    assert_eq!(
        dep["metadata"]["annotations"]["aetherlynx.io/model"],
        "gpt-4o-mini"
    );
    let svc = api.get("services", "aetherlynx-student-agent").unwrap();
    let owner = &svc["metadata"]["ownerReferences"][0];
    assert_eq!(owner["kind"], "Deployment");
    assert_eq!(owner["uid"], dep["metadata"]["uid"]);
    assert_eq!(owner["controller"], true);
    assert_eq!(
        svc["spec"]["selector"],
        dep["spec"]["selector"]["matchLabels"]
    );
}

#[tokio::test]
async fn pod_template_is_hardened_and_carries_the_model() {
    let api = FakeApi::default();
    api.runtime()
        .apply(&want("student-agent", 1))
        .await
        .unwrap();
    let dep = api.get("deployments", "aetherlynx-student-agent").unwrap();
    let pod = &dep["spec"]["template"]["spec"];
    let c = &pod["containers"][0];

    assert_eq!(pod["automountServiceAccountToken"], false);
    assert_eq!(pod["serviceAccountName"], "university-agent");
    assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
    assert_eq!(
        pod["securityContext"]["seccompProfile"]["type"],
        "RuntimeDefault"
    );
    assert_eq!(c["securityContext"]["readOnlyRootFilesystem"], true);
    assert_eq!(c["securityContext"]["allowPrivilegeEscalation"], false);
    assert_eq!(c["securityContext"]["capabilities"]["drop"], json!(["ALL"]));
    assert_eq!(c["resources"]["limits"]["memory"], "512Mi");
    assert_eq!(c["readinessProbe"]["httpGet"]["path"], "/readyz");
    assert_eq!(c["image"], "registry.example.com/agent:0.2.0");
    let env: BTreeMap<String, String> = c["env"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| Some((e["name"].as_str()?.into(), e["value"].as_str()?.into())))
        .collect();
    assert_eq!(env["AETHERLYNX_LLM_MODEL"], "gpt-4o-mini");
    assert_eq!(env["AETHERLYNX_AGENT_NAME"], "student-agent");
    assert_eq!(
        c["envFrom"][0]["configMapRef"]["name"],
        "university-agent-config"
    );
    assert_eq!(
        c["envFrom"][1]["secretRef"]["name"],
        "university-agent-secrets"
    );
    // Telemetría: la cabecera viene de un Secret opcional (nunca en claro en
    // el manifiesto) y la CA del colector de un ConfigMap opcional.
    let headers = c["env"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "OTEL_EXPORTER_OTLP_HEADERS")
        .unwrap();
    assert!(headers.get("value").is_none());
    assert_eq!(
        headers["valueFrom"]["secretKeyRef"],
        json!({"name": "aetherlynx-telemetry-ingest", "key": "otlp-headers", "optional": true})
    );
    let ca = pod["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "telemetry-ca")
        .unwrap();
    assert_eq!(ca["configMap"]["name"], "aetherlynx-telemetry-ca");
    assert_eq!(ca["configMap"]["optional"], true);
    assert_eq!(
        c["envFrom"][2]["configMapRef"],
        json!({"name": "aetherlynx-telemetry-client", "optional": true})
    );
    assert!(c["volumeMounts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["mountPath"] == "/etc/aetherlynx/telemetry-ca" && m["readOnly"] == true));
    // La NetworkPolicy de los agentes selecciona por este componente.
    assert_eq!(
        dep["spec"]["template"]["metadata"]["labels"]["app.kubernetes.io/component"],
        "llm-agent"
    );
}

#[tokio::test]
async fn ready_only_after_the_rollout_is_observed_and_complete() {
    let api = FakeApi::default();
    let rt = api.runtime();
    rt.apply(&want("a", 3)).await.unwrap();
    api.finish_rollout("aetherlynx-a", 3);
    assert_eq!(
        rt.apply(&want("a", 3)).await.unwrap(),
        Observation {
            ready: 3,
            converged: true
        }
    );

    // Escalar a 5 sube la generación; el estado anterior (3 listas) ya no
    // cuenta como convergido aunque las réplicas listas no hayan cambiado.
    assert_eq!(
        rt.apply(&want("a", 5)).await.unwrap(),
        Observation {
            ready: 3,
            converged: false
        }
    );
    api.finish_rollout("aetherlynx-a", 5);
    assert_eq!(
        rt.apply(&want("a", 5)).await.unwrap(),
        Observation {
            ready: 5,
            converged: true
        }
    );

    // Pods viejos aún vivos: 5 listas pero solo 4 actualizadas.
    let g = api.get("deployments", "aetherlynx-a").unwrap()["metadata"]["generation"].clone();
    api.set_status(
        "aetherlynx-a",
        json!({"observedGeneration": g, "replicas": 6, "updatedReplicas": 4, "readyReplicas": 5}),
    );
    assert!(!rt.apply(&want("a", 5)).await.unwrap().converged);

    // Rollout de una versión nueva a mitad de camino: el total y las listas
    // cuadran, pero 2 de las 5 réplicas todavía son de la versión anterior.
    api.set_status(
        "aetherlynx-a",
        json!({"observedGeneration": g, "replicas": 5, "updatedReplicas": 3, "readyReplicas": 5}),
    );
    assert!(!rt.apply(&want("a", 5)).await.unwrap().converged);
}

#[tokio::test]
async fn reapplying_an_unchanged_agent_does_not_bump_the_generation() {
    let api = FakeApi::default();
    let rt = api.runtime();
    rt.apply(&want("a", 2)).await.unwrap();
    rt.apply(&want("a", 2)).await.unwrap();
    assert_eq!(
        api.get("deployments", "aetherlynx-a").unwrap()["metadata"]["generation"],
        1
    );
}

#[tokio::test]
async fn stuck_rollouts_and_quota_errors_are_reported() {
    let api = FakeApi::default();
    let rt = api.runtime();
    rt.apply(&want("a", 2)).await.unwrap();
    let g = api.get("deployments", "aetherlynx-a").unwrap()["metadata"]["generation"].clone();
    api.set_status(
        "aetherlynx-a",
        json!({"observedGeneration": g, "conditions": [{"type": "Progressing", "status": "False",
               "reason": "ProgressDeadlineExceeded", "message": "ReplicaSet \"a-1\" has timed out progressing."}]}),
    );
    let err = rt.apply(&want("a", 2)).await.unwrap_err();
    assert!(
        err.contains("no progresa") && err.contains("timed out"),
        "{err}"
    );

    api.set_status(
        "aetherlynx-a",
        json!({"observedGeneration": g, "conditions": [{"type": "ReplicaFailure", "status": "True", "message": "exceeded quota: compute"}]}),
    );
    assert!(rt
        .apply(&want("a", 2))
        .await
        .unwrap_err()
        .contains("exceeded quota"));
}

#[tokio::test]
async fn conditions_from_a_previous_rollout_are_ignored() {
    let api = FakeApi::default();
    let rt = api.runtime();
    rt.apply(&want("a", 2)).await.unwrap();
    // El rollout anterior (generación 1) se atascó...
    api.set_status(
        "aetherlynx-a",
        json!({"observedGeneration": 1, "conditions": [{"type": "Progressing", "status": "False", "message": "viejo"}]}),
    );
    // ...y el usuario lo corrige escalando: generación 2, aún no observada.
    let obs = rt.apply(&want("a", 3)).await.unwrap();
    assert_eq!(
        obs,
        Observation {
            ready: 0,
            converged: false
        }
    );
}

#[tokio::test]
async fn delete_waits_for_kubernetes_to_finish() {
    let api = FakeApi::default();
    api.0.lock().unwrap().slow_delete = true;
    let rt = api.runtime();
    rt.apply(&want("a", 2)).await.unwrap();
    api.finish_rollout("aetherlynx-a", 2);

    let del = DesiredState {
        deleting: true,
        ..want("a", 0)
    };
    assert_eq!(
        rt.apply(&del).await.unwrap(),
        Observation {
            ready: 2,
            converged: false
        }
    );
    let d = api
        .calls()
        .into_iter()
        .find(|c| c.method == "DELETE")
        .unwrap();
    assert_eq!(
        d.path,
        "/apis/apps/v1/namespaces/aetherlynx/deployments/aetherlynx-a"
    );
    assert_eq!(d.body["propagationPolicy"], "Foreground");
    assert_eq!(
        d.body["preconditions"]["uid"],
        api.get("deployments", "aetherlynx-a").unwrap()["metadata"]["uid"]
    );
    let mark = api.calls().into_iter().position(|c| {
        c.method == "PATCH" && c.body["metadata"]["annotations"]["aetherlynx.io/deleting"] == "true"
    });
    let del_pos = api.calls().into_iter().position(|c| c.method == "DELETE");
    assert!(
        mark.is_some() && mark < del_pos,
        "la lápida se pone antes del DELETE"
    );

    api.finalize_deletes();
    assert_eq!(
        rt.apply(&del).await.unwrap(),
        Observation {
            ready: 0,
            converged: true
        }
    );
    // Borrar algo que ya no existe no es un error.
    assert_eq!(
        rt.apply(&del).await.unwrap(),
        Observation {
            ready: 0,
            converged: true
        }
    );
}

#[tokio::test]
async fn never_touches_objects_it_does_not_own() {
    let api = FakeApi::default();
    let rt = api.runtime();
    // Un objeto ajeno con el nombre que tendría el agente "orchestrator"
    // (el propio Deployment del orquestador, por ejemplo).
    let foreign = json!({"apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "aetherlynx-orchestrator", "uid": "uid-own", "generation": 1,
                     "labels": {"app.kubernetes.io/name": "aetherlynx-orchestrator"}},
        "spec": {"replicas": 1, "selector": {"matchLabels": {"app.kubernetes.io/name": "aetherlynx-orchestrator"}}}});
    api.insert("deployments", foreign.clone());

    let err = rt.apply(&want("orchestrator", 2)).await.unwrap_err();
    assert!(err.contains("no lo gestiona AetherLynx"), "{err}");
    let del = DesiredState {
        deleting: true,
        ..want("orchestrator", 0)
    };
    assert_eq!(
        rt.apply(&del).await.unwrap(),
        Observation {
            ready: 0,
            converged: true
        }
    );
    assert_eq!(
        api.get("deployments", "aetherlynx-orchestrator").unwrap(),
        foreign
    );
    assert!(
        api.calls().iter().all(|c| c.method == "GET"),
        "solo lecturas sobre lo ajeno"
    );

    // Lo mismo con un agente de otra instancia del orquestador.
    let mut other = foreign.clone();
    other["metadata"]["name"] = json!("aetherlynx-shared");
    other["metadata"]["labels"] = json!({"app.kubernetes.io/managed-by": "aetherlynx-orchestrator",
        "aetherlynx.io/orchestrator": "otra", "aetherlynx.io/agent": "shared"});
    api.insert("deployments", other.clone());
    let err = rt.apply(&want("shared", 1)).await.unwrap_err();
    assert!(err.contains("instancia 'otra'"), "{err}");
    rt.apply(&DesiredState {
        deleting: true,
        ..want("shared", 0)
    })
    .await
    .unwrap();
    assert_eq!(api.get("deployments", "aetherlynx-shared").unwrap(), other);
}

#[tokio::test]
async fn recover_and_gc_never_delete_what_could_not_be_recovered() {
    let api = FakeApi::default();
    let rt = api.runtime();
    rt.apply(&want("keep", 4)).await.unwrap();
    rt.apply(&want("huge", 1)).await.unwrap();
    rt.apply(&want("nomodel", 1)).await.unwrap();
    {
        let mut st = api.0.lock().unwrap();
        let huge = st
            .objects
            .get_mut(&("deployments".into(), "aetherlynx-huge".into()))
            .unwrap();
        huge["spec"]["replicas"] = json!(500); // alguien hizo `kubectl scale`
        let nm = st
            .objects
            .get_mut(&("deployments".into(), "aetherlynx-nomodel".into()))
            .unwrap();
        nm["metadata"]["annotations"] = json!({}); // alguien borró la anotación
    }
    // Ajenos: de otra instancia y no gestionado.
    let mut other = api.get("deployments", "aetherlynx-keep").unwrap();
    other["metadata"]["name"] = json!("aetherlynx-foreign");
    other["metadata"]["labels"]["aetherlynx.io/orchestrator"] = json!("otro");
    other["metadata"]["labels"]["aetherlynx.io/agent"] = json!("foreign");
    api.insert("deployments", other);
    api.insert(
        "deployments",
        json!({"metadata": {"name": "postgres", "labels": {"app": "db"}}, "spec": {"replicas": 1}}),
    );

    let mut found = rt.recover().await.unwrap();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    let got: Vec<_> = found
        .iter()
        .map(|r| (r.name.as_str(), r.desired_replicas))
        .collect();
    assert_eq!(
        got,
        vec![("huge", 100), ("keep", 4)],
        "réplicas acotadas; sin modelo no se recupera"
    );

    // Ni siquiera con un registro vacío borra nada: nada tiene lápida.
    assert_eq!(rt.garbage_collect(&[]).await.unwrap(), 0);
    for name in [
        "aetherlynx-keep",
        "aetherlynx-huge",
        "aetherlynx-nomodel",
        "aetherlynx-foreign",
        "postgres",
    ] {
        assert!(
            api.get("deployments", name).is_some(),
            "{name} no debía borrarse"
        );
    }
    assert!(api.calls().iter().all(|c| c.method != "DELETE"));
}

#[tokio::test]
async fn an_interrupted_delete_completes_after_a_restart() {
    let api = FakeApi::default();
    let rt = api.runtime();
    rt.apply(&want("a", 2)).await.unwrap();
    api.0.lock().unwrap().fail_deletes = 1;
    let del = DesiredState {
        deleting: true,
        ..want("a", 0)
    };
    assert!(rt.apply(&del).await.is_err(), "el DELETE falló");
    let dep = api.get("deployments", "aetherlynx-a").unwrap();
    assert_eq!(
        dep["metadata"]["annotations"]["aetherlynx.io/deleting"], "true",
        "queda la lápida"
    );

    // El orquestador se reinicia antes de reintentar: no recupera el agente...
    let rt2 = api.runtime();
    assert!(rt2.recover().await.unwrap().is_empty());
    // ...y la limpieza termina el borrado.
    assert_eq!(rt2.garbage_collect(&[]).await.unwrap(), 1);
    assert!(api.get("deployments", "aetherlynx-a").is_none());
}

#[tokio::test]
async fn recreating_an_agent_clears_a_leftover_tombstone() {
    let api = FakeApi::default();
    let rt = api.runtime();
    rt.apply(&want("a", 1)).await.unwrap();
    api.0.lock().unwrap().fail_deletes = 1;
    let _ = rt
        .apply(&DesiredState {
            deleting: true,
            ..want("a", 0)
        })
        .await;
    // El usuario vuelve a crear el agente con el mismo nombre.
    rt.apply(&want("a", 1)).await.unwrap();
    let dep = api.get("deployments", "aetherlynx-a").unwrap();
    assert!(dep["metadata"]["annotations"]
        .get("aetherlynx.io/deleting")
        .is_none());
    assert_eq!(rt.garbage_collect(&["a".into()]).await.unwrap(), 0);
    assert_eq!(
        rt.garbage_collect(&[]).await.unwrap(),
        0,
        "sin lápida no se borra"
    );
    assert!(api.get("deployments", "aetherlynx-a").is_some());
}

#[tokio::test]
async fn the_service_is_applied_once_per_deployment() {
    let api = FakeApi::default();
    let rt = api.runtime();
    for _ in 0..4 {
        rt.apply(&want("a", 1)).await.unwrap();
    }
    let svc_patches = api
        .calls()
        .iter()
        .filter(|c| c.method == "PATCH" && c.path.contains("/services/"))
        .count();
    assert_eq!(svc_patches, 1);
    let svc = api.get("services", "aetherlynx-a").unwrap();
    assert!(svc["metadata"]["ownerReferences"][0]
        .get("blockOwnerDeletion")
        .is_none());
}

#[tokio::test]
async fn permission_errors_are_readable() {
    let api = FakeApi::default();
    api.0.lock().unwrap().forbid = true;
    let err = api.runtime().apply(&want("a", 1)).await.unwrap_err();
    assert!(
        err.contains("sin permisos") && err.contains("Role"),
        "{err}"
    );
}

#[test]
fn config_validation() {
    let mut c = KubeRuntimeConfig::new("");
    assert!(c.validate().is_err());
    c.image = "registry.example.com/agent:0.2.0".into();
    assert!(c.validate().is_ok());
    c.instance = "bad value!".into();
    assert!(c.validate().is_err());
    c.instance = "default".into();
    c.memory_limit = "512Mi; rm".into();
    assert!(c.validate().is_err());
}

#[tokio::test]
async fn full_lifecycle_through_the_reconciler_and_restart_recovery() {
    let api = FakeApi::default();
    let registry = Arc::new(Registry::new());
    let rec = Reconciler::new(registry.clone(), Arc::new(api.runtime()));

    registry.create("student-agent", "gpt-4o-mini", 3).unwrap();
    rec.reconcile_once().await;
    assert_eq!(
        registry.get("student-agent").unwrap().phase,
        AgentPhase::Scaling
    );
    api.finish_rollout("aetherlynx-student-agent", 3);
    rec.reconcile_once().await;
    let a = registry.get("student-agent").unwrap();
    assert_eq!((a.phase, a.ready_replicas), (AgentPhase::Ready, 3));

    // El orquestador se reinicia: registro vacío, pero el Deployment sigue.
    let registry2 = Arc::new(Registry::new());
    let rec2 = Reconciler::new(registry2.clone(), Arc::new(api.runtime()));
    assert_eq!(rec2.recover().await.unwrap(), 1);
    rec2.reconcile_once().await;
    let a = registry2.get("student-agent").unwrap();
    assert_eq!(
        (a.phase, a.desired_replicas, a.model.as_str()),
        (AgentPhase::Ready, 3, "gpt-4o-mini")
    );

    registry2.delete("student-agent").unwrap();
    rec2.reconcile_once().await;
    assert!(registry2.list().is_empty());
    assert!(api.get("deployments", "aetherlynx-student-agent").is_none());
}
