//! Runtime de Kubernetes: cada agente es un Deployment y un Service.
//!
//! - Se usa *server-side apply* con el gestor de campos
//!   `aetherlynx-orchestrator` y `force`: el orquestador es el dueño
//!   declarado de los campos que aplica. Cambios manuales a esos campos (por
//!   ejemplo `kubectl scale`) se revierten en el siguiente ciclo.
//! - **Propiedad.** Nunca se modifica ni se borra un objeto que no lleve las
//!   etiquetas de esta instancia del orquestador, aunque tenga el nombre
//!   esperado. Los borrados llevan precondición de `uid`.
//! - El Service tiene al Deployment como propietario (`ownerReferences`):
//!   si el Deployment se borra, Kubernetes borra el Service.
//! - **Estado durable.** Al arrancar, el orquestador lista sus Deployments
//!   para reconstruir el registro. Antes de borrar uno lo marca con la
//!   anotación `aetherlynx.io/deleting` (la "lápida"), así un borrado
//!   interrumpido por un reinicio se completa después y el agente no vuelve.
//!   La limpieza solo borra lo que tiene lápida.
//! - Un agente cuenta como listo solo cuando el rollout terminó: generación
//!   observada al día y todas las réplicas actualizadas y listas.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use k8s_openapi::api::apps::v1::{
    Deployment, DeploymentSpec, DeploymentStrategy, RollingUpdateDeployment,
};
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMapEnvSource, ConfigMapVolumeSource, Container, ContainerPort,
    EmptyDirVolumeSource, EnvFromSource, EnvVar, EnvVarSource, HTTPGetAction, PodSecurityContext,
    PodSpec, PodTemplateSpec, Probe, ResourceRequirements, SeccompProfile, SecretEnvSource,
    SecretKeySelector, SecurityContext, Service, ServicePort, ServiceSpec,
    TopologySpreadConstraint, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{
    Api, DeleteParams, ListParams, Patch, PatchParams, Preconditions, PropagationPolicy,
};
use kube::Client;

use crate::runtime::{DesiredState, Observation, RecoveredAgent, Runtime};
use crate::validation::MAX_REPLICAS;

/// Prefijo de los objetos: el Deployment y el Service del agente `x` se
/// llaman `aetherlynx-x`. Por eso los nombres de agente admiten como máximo
/// 52 caracteres (63 - 11).
pub const NAME_PREFIX: &str = "aetherlynx-";
pub const FIELD_MANAGER: &str = "aetherlynx-orchestrator";

const L_NAME: &str = "app.kubernetes.io/name";
const L_COMPONENT: &str = "app.kubernetes.io/component";
const L_PART_OF: &str = "app.kubernetes.io/part-of";
const L_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
const L_INSTANCE: &str = "aetherlynx.io/orchestrator";
const L_AGENT: &str = "aetherlynx.io/agent";
const A_MODEL: &str = "aetherlynx.io/model";
const A_DELETING: &str = "aetherlynx.io/deleting";

/// Donde los pods de agente ven la CA del colector de telemetría.
pub const TELEMETRY_CA_DIR: &str = "/etc/aetherlynx/telemetry-ca";

#[derive(Debug, Clone)]
pub struct KubeRuntimeConfig {
    /// Namespace de los agentes. `None`: el del cliente (en el clúster, el
    /// del propio orquestador).
    pub namespace: Option<String>,
    /// Imagen del contenedor del agente. Fije una versión, no `latest`.
    pub image: String,
    /// Distingue a varios orquestadores en un mismo namespace.
    pub instance: String,
    pub env_from_secret: Option<String>,
    pub env_from_configmap: Option<String>,
    pub service_account: Option<String>,
    /// Prefijo de los objetos de telemetría que reciben los pods, todos
    /// opcionales (sin ellos el agente arranca y simplemente no exporta):
    /// ConfigMap `<p>-client` (variables `OTEL_*`), Secret `<p>-ingest`
    /// (clave `otlp-headers`) y ConfigMap `<p>-ca` (`ca.crt`, montado en
    /// [`TELEMETRY_CA_DIR`]).
    pub telemetry: Option<String>,
    pub container_port: u16,
    pub cpu_request: String,
    pub memory_request: String,
    pub memory_limit: String,
}

impl KubeRuntimeConfig {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            namespace: None,
            image: image.into(),
            instance: "default".into(),
            env_from_secret: Some("university-agent-secrets".into()),
            env_from_configmap: Some("university-agent-config".into()),
            service_account: Some("university-agent".into()),
            telemetry: Some("aetherlynx-telemetry".into()),
            container_port: 5000,
            cpu_request: "100m".into(),
            memory_request: "192Mi".into(),
            memory_limit: "512Mi".into(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let img = self.image.trim();
        if img.is_empty() || img.chars().any(char::is_whitespace) {
            return Err("la imagen del agente es obligatoria y no puede tener espacios".into());
        }
        let last = img.rsplit('/').next().unwrap_or(img);
        if !img.contains('@') && (!last.contains(':') || last.ends_with(":latest")) {
            tracing::warn!(
                image = %img,
                "la imagen del agente no fija una versión; use una etiqueta concreta o un digest"
            );
        }
        let label_ok = |v: &str| {
            !v.is_empty()
                && v.len() <= 63
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
                && v.as_bytes()[0].is_ascii_alphanumeric()
                && v.as_bytes()[v.len() - 1].is_ascii_alphanumeric()
        };
        if !label_ok(&self.instance) {
            return Err(
                "la instancia debe ser un valor de etiqueta válido (1-63, alfanumérico, - _ .)"
                    .into(),
            );
        }
        for (what, q) in [
            ("cpu_request", &self.cpu_request),
            ("memory_request", &self.memory_request),
            ("memory_limit", &self.memory_limit),
        ] {
            if q.is_empty() || !q.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.') {
                return Err(format!("{what} no es una cantidad válida: {q:?}"));
            }
        }
        Ok(())
    }
}

pub struct KubernetesRuntime {
    deployments: Api<Deployment>,
    services: Api<Service>,
    t: AgentTemplates,
    /// agente -> uid del Deployment para el que ya se aplicó su Service.
    /// Evita re-aplicar el Service en cada ciclo.
    service_owner: Mutex<HashMap<String, String>>,
    /// Objetos sobre los que ya se avisó, para no repetir el aviso cada ciclo.
    warned: Mutex<HashSet<String>>,
}

/// Construye los objetos de Kubernetes de un agente. No habla con el API:
/// sirve también para `aetherlynx-orchestrator render-agent`.
pub struct AgentTemplates {
    cfg: KubeRuntimeConfig,
    namespace: String,
}

impl AgentTemplates {
    pub fn new(cfg: KubeRuntimeConfig, namespace: impl Into<String>) -> Result<Self, String> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            namespace: namespace.into(),
        })
    }

    fn selector_labels(&self, agent: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            (L_NAME.into(), "aetherlynx-agent".into()),
            (L_INSTANCE.into(), self.cfg.instance.clone()),
            (L_AGENT.into(), agent.into()),
        ])
    }

    fn labels(&self, agent: &str) -> BTreeMap<String, String> {
        let mut l = self.selector_labels(agent);
        l.insert(L_COMPONENT.into(), "llm-agent".into());
        l.insert(L_PART_OF.into(), "aetherlynx".into());
        l.insert(L_MANAGED_BY.into(), FIELD_MANAGER.into());
        l
    }

    /// ¿Es este objeto de esta instancia y de este agente?
    pub fn owns(&self, meta: &ObjectMeta, agent: &str) -> bool {
        let Some(l) = meta.labels.as_ref() else {
            return false;
        };
        l.get(L_MANAGED_BY).map(String::as_str) == Some(FIELD_MANAGER)
            && l.get(L_INSTANCE) == Some(&self.cfg.instance)
            && l.get(L_AGENT).map(String::as_str) == Some(agent)
    }

    fn ownership_selector(&self) -> String {
        format!(
            "{L_MANAGED_BY}={FIELD_MANAGER},{L_INSTANCE}={}",
            self.cfg.instance
        )
    }

    pub fn deployment_manifest(&self, d: &DesiredState<'_>) -> Deployment {
        let name = object_name(d.name);
        let labels = self.labels(d.name);
        let port = i32::from(self.cfg.container_port);
        let http_probe = |path: &str, period: i32, failures: i32| Probe {
            http_get: Some(HTTPGetAction {
                path: Some(path.into()),
                port: IntOrString::String("http".into()),
                ..Default::default()
            }),
            period_seconds: Some(period),
            timeout_seconds: Some(3),
            failure_threshold: Some(failures),
            ..Default::default()
        };
        let mut env_from = Vec::new();
        if let Some(cm) = &self.cfg.env_from_configmap {
            env_from.push(EnvFromSource {
                config_map_ref: Some(ConfigMapEnvSource {
                    name: cm.clone(),
                    optional: Some(false),
                }),
                ..Default::default()
            });
        }
        if let Some(sec) = &self.cfg.env_from_secret {
            env_from.push(EnvFromSource {
                secret_ref: Some(SecretEnvSource {
                    name: sec.clone(),
                    optional: Some(false),
                }),
                ..Default::default()
            });
        }
        let mut env = vec![
            EnvVar {
                name: "AETHERLYNX_AGENT_NAME".into(),
                value: Some(d.name.into()),
                ..Default::default()
            },
            EnvVar {
                name: "AETHERLYNX_LLM_MODEL".into(),
                value: Some(d.model.into()),
                ..Default::default()
            },
        ];
        if let Some(p) = &self.cfg.telemetry {
            env_from.push(EnvFromSource {
                config_map_ref: Some(ConfigMapEnvSource {
                    name: format!("{p}-client"),
                    optional: Some(true),
                }),
                ..Default::default()
            });
            env.push(EnvVar {
                name: "OTEL_EXPORTER_OTLP_HEADERS".into(),
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: format!("{p}-ingest"),
                        key: "otlp-headers".into(),
                        optional: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        let mut mounts = vec![VolumeMount {
            name: "tmp".into(),
            mount_path: "/tmp".into(),
            ..Default::default()
        }];
        let mut volumes = vec![Volume {
            name: "tmp".into(),
            empty_dir: Some(EmptyDirVolumeSource {
                size_limit: Some(Quantity("64Mi".into())),
                ..Default::default()
            }),
            ..Default::default()
        }];
        if let Some(p) = &self.cfg.telemetry {
            mounts.push(VolumeMount {
                name: "telemetry-ca".into(),
                mount_path: TELEMETRY_CA_DIR.into(),
                read_only: Some(true),
                ..Default::default()
            });
            volumes.push(Volume {
                name: "telemetry-ca".into(),
                config_map: Some(ConfigMapVolumeSource {
                    name: format!("{p}-ca"),
                    optional: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        let container = Container {
            name: "agent".into(),
            image: Some(self.cfg.image.clone()),
            image_pull_policy: Some("IfNotPresent".into()),
            ports: Some(vec![ContainerPort {
                name: Some("http".into()),
                container_port: port,
                ..Default::default()
            }]),
            // `env` tiene prioridad sobre `envFrom`: el modelo del agente gana
            // al valor general del ConfigMap.
            env: Some(env),
            env_from: (!env_from.is_empty()).then_some(env_from),
            security_context: Some(SecurityContext {
                allow_privilege_escalation: Some(false),
                read_only_root_filesystem: Some(true),
                capabilities: Some(Capabilities {
                    drop: Some(vec!["ALL".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            resources: Some(ResourceRequirements {
                requests: Some(BTreeMap::from([
                    ("cpu".into(), Quantity(self.cfg.cpu_request.clone())),
                    ("memory".into(), Quantity(self.cfg.memory_request.clone())),
                ])),
                limits: Some(BTreeMap::from([(
                    "memory".into(),
                    Quantity(self.cfg.memory_limit.clone()),
                )])),
                ..Default::default()
            }),
            startup_probe: Some(http_probe("/healthz", 2, 30)),
            liveness_probe: Some(http_probe("/healthz", 15, 3)),
            readiness_probe: Some(http_probe("/readyz", 10, 3)),
            volume_mounts: Some(mounts),
            ..Default::default()
        };
        Deployment {
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels.clone()),
                annotations: Some(BTreeMap::from([(A_MODEL.into(), d.model.into())])),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(d.desired_replicas as i32),
                revision_history_limit: Some(5),
                selector: LabelSelector {
                    match_labels: Some(self.selector_labels(d.name)),
                    ..Default::default()
                },
                strategy: Some(DeploymentStrategy {
                    type_: Some("RollingUpdate".into()),
                    rolling_update: Some(RollingUpdateDeployment {
                        max_unavailable: Some(IntOrString::Int(0)),
                        max_surge: Some(IntOrString::Int(1)),
                    }),
                }),
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        service_account_name: self.cfg.service_account.clone(),
                        automount_service_account_token: Some(false),
                        security_context: Some(PodSecurityContext {
                            run_as_non_root: Some(true),
                            run_as_user: Some(10001),
                            run_as_group: Some(10001),
                            fs_group: Some(10001),
                            seccomp_profile: Some(SeccompProfile {
                                type_: "RuntimeDefault".into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        topology_spread_constraints: Some(vec![TopologySpreadConstraint {
                            max_skew: 1,
                            topology_key: "kubernetes.io/hostname".into(),
                            when_unsatisfiable: "ScheduleAnyway".into(),
                            label_selector: Some(LabelSelector {
                                match_labels: Some(self.selector_labels(d.name)),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }]),
                        containers: vec![container],
                        volumes: Some(volumes),
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// `owner_uid`: uid del Deployment propietario. `None` solo para mostrar el
    /// manifiesto: aplicado con un uid falso, el recolector de basura lo borraría.
    pub fn service_manifest(&self, d: &DesiredState<'_>, owner_uid: Option<&str>) -> Service {
        let name = object_name(d.name);
        Service {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(self.labels(d.name)),
                owner_references: owner_uid.map(|uid| {
                    vec![OwnerReference {
                        api_version: "apps/v1".into(),
                        kind: "Deployment".into(),
                        name,
                        uid: uid.into(),
                        controller: Some(true),
                        // Sin blockOwnerDeletion: exigiría permiso sobre
                        // deployments/finalizers en clústeres con
                        // OwnerReferencesPermissionEnforcement (p. ej. OpenShift).
                        block_owner_deletion: None,
                    }]
                }),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(self.selector_labels(d.name)),
                ports: Some(vec![ServicePort {
                    name: Some("http".into()),
                    port: 80,
                    target_port: Some(IntOrString::String("http".into())),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

pub fn object_name(agent: &str) -> String {
    format!("{NAME_PREFIX}{agent}")
}

fn describe(e: kube::Error) -> String {
    match e {
        kube::Error::Api(ae) if ae.code == 403 => {
            format!(
                "sin permisos en el API de Kubernetes (revise el Role del orquestador): {}",
                ae.message
            )
        }
        kube::Error::Api(ae) if ae.code == 422 => {
            format!("Kubernetes rechazó el objeto: {}", ae.message)
        }
        kube::Error::Api(ae) => format!("API de Kubernetes ({}): {}", ae.code, ae.message),
        other => format!("no se pudo hablar con el API de Kubernetes: {other}"),
    }
}

fn is_not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(ae) if ae.code == 404)
}

fn count(v: Option<i32>) -> u32 {
    v.unwrap_or(0).max(0) as u32
}

/// Traduce el estado de un Deployment a una observación.
pub fn observe_deployment(dep: &Deployment, desired: u32) -> Result<Observation, String> {
    let st = dep.status.clone().unwrap_or_default();
    let ready = count(st.ready_replicas);
    let generation = dep.metadata.generation.unwrap_or(0);
    // Hasta que el controlador observa la generación actual, las condiciones
    // describen el rollout anterior: no se usan para declarar un fallo.
    if st.observed_generation.unwrap_or(0) < generation {
        return Ok(Observation {
            ready,
            converged: false,
        });
    }
    for c in st.conditions.iter().flatten() {
        let msg = c.message.clone().unwrap_or_default();
        if c.type_ == "Progressing" && c.status == "False" {
            return Err(format!("el rollout no progresa: {msg}"));
        }
        if c.type_ == "ReplicaFailure" && c.status == "True" {
            return Err(format!("no se pudieron crear réplicas: {msg}"));
        }
    }
    let converged =
        ready == desired && count(st.updated_replicas) == desired && count(st.replicas) == desired;
    Ok(Observation { ready, converged })
}

fn is_tombstoned(meta: &ObjectMeta) -> bool {
    meta.annotations
        .as_ref()
        .is_some_and(|a| a.get(A_DELETING).map(String::as_str) == Some("true"))
}

impl KubernetesRuntime {
    /// Conecta con la configuración estándar: dentro del clúster usa la
    /// cuenta de servicio del pod; fuera, `KUBECONFIG` o `~/.kube/config`.
    pub async fn connect(cfg: KubeRuntimeConfig) -> Result<Self, String> {
        let client = Client::try_default()
            .await
            .map_err(|e| format!("no se encontró configuración de Kubernetes: {e}"))?;
        Self::with_client(client, cfg)
    }

    pub fn with_client(client: Client, cfg: KubeRuntimeConfig) -> Result<Self, String> {
        let namespace = cfg
            .namespace
            .clone()
            .unwrap_or_else(|| client.default_namespace().to_owned());
        Ok(Self {
            deployments: Api::namespaced(client.clone(), &namespace),
            services: Api::namespaced(client, &namespace),
            t: AgentTemplates::new(cfg, namespace)?,
            service_owner: Mutex::new(HashMap::new()),
            warned: Mutex::new(HashSet::new()),
        })
    }

    pub fn namespace(&self) -> &str {
        &self.t.namespace
    }

    fn not_ours(&self, kind: &str, name: &str, meta: &ObjectMeta) -> String {
        let owner = meta
            .labels
            .as_ref()
            .and_then(|l| l.get(L_INSTANCE))
            .map(|i| format!("lo gestiona la instancia '{i}'"))
            .unwrap_or_else(|| "no lo gestiona AetherLynx".into());
        format!("ya existe el {kind} '{name}' y {owner}; no se modifica. Use otro nombre de agente")
    }

    fn warn_once(&self, key: String, msg: &str) {
        let mut w = self.warned.lock().unwrap_or_else(|p| p.into_inner());
        if w.insert(key.clone()) {
            tracing::warn!(object = %key, "{msg}");
        }
    }

    /// Marca el Deployment con la lápida y lo borra. La lápida hace que la
    /// intención de borrar sobreviva a un reinicio del orquestador: al
    /// arrancar no se recupera como agente y la limpieza termina el trabajo.
    async fn retire(&self, dep: &Deployment) -> Result<(), String> {
        let name = dep.metadata.name.clone().unwrap_or_default();
        if !is_tombstoned(&dep.metadata) {
            let mark = serde_json::json!({"metadata": {"annotations": {A_DELETING: "true"}}});
            self.deployments
                .patch(&name, &PatchParams::default(), &Patch::Merge(&mark))
                .await
                .map_err(describe)?;
        }
        let params = DeleteParams {
            propagation_policy: Some(PropagationPolicy::Foreground),
            // Solo este objeto: si alguien lo recreó con el mismo nombre,
            // el borrado falla en vez de llevarse el nuevo.
            preconditions: dep.metadata.uid.clone().map(|uid| Preconditions {
                uid: Some(uid),
                resource_version: None,
            }),
            ..Default::default()
        };
        match self.deployments.delete(&name, &params).await {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(describe(e)),
        }
    }

    async fn delete_agent(&self, agent: &str) -> Result<Observation, String> {
        let name = object_name(agent);
        self.service_owner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(agent);
        let Some(dep) = self.deployments.get_opt(&name).await.map_err(describe)? else {
            return Ok(Observation {
                ready: 0,
                converged: true,
            });
        };
        if !self.t.owns(&dep.metadata, agent) {
            // Nunca se borra lo ajeno. El agente se da por eliminado: lo que
            // existe con ese nombre no era suyo.
            tracing::warn!(object = %name, "{}", self.not_ours("Deployment", &name, &dep.metadata));
            return Ok(Observation {
                ready: 0,
                converged: true,
            });
        }
        let ready = count(dep.status.as_ref().and_then(|s| s.ready_replicas));
        if dep.metadata.deletion_timestamp.is_none() {
            self.retire(&dep).await?;
        }
        // El Service se va con el Deployment (ownerReferences). Si alguien le
        // quitó esa referencia, se borra aquí, pero solo si es nuestro.
        if let Some(svc) = self.services.get_opt(&name).await.map_err(describe)? {
            if self.t.owns(&svc.metadata, agent) && svc.metadata.deletion_timestamp.is_none() {
                match self.services.delete(&name, &DeleteParams::default()).await {
                    Ok(_) => {}
                    Err(e) if is_not_found(&e) => {}
                    Err(e) => return Err(describe(e)),
                }
            }
        }
        match self.deployments.get_opt(&name).await.map_err(describe)? {
            None => Ok(Observation {
                ready: 0,
                converged: true,
            }),
            Some(_) => Ok(Observation {
                ready,
                converged: false,
            }),
        }
    }

    async fn ensure_service(&self, d: &DesiredState<'_>, dep_uid: &str) -> Result<(), String> {
        let cached = self
            .service_owner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(d.name)
            .cloned();
        if cached.as_deref() == Some(dep_uid) {
            return Ok(()); // ya aplicado para este Deployment
        }
        let name = object_name(d.name);
        if let Some(svc) = self.services.get_opt(&name).await.map_err(describe)? {
            if !self.t.owns(&svc.metadata, d.name) {
                return Err(self.not_ours("Service", &name, &svc.metadata));
            }
        }
        let pp = PatchParams::apply(FIELD_MANAGER).force();
        self.services
            .patch(
                &name,
                &pp,
                &Patch::Apply(&self.t.service_manifest(d, Some(dep_uid))),
            )
            .await
            .map_err(describe)?;
        self.service_owner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(d.name.to_owned(), dep_uid.to_owned());
        Ok(())
    }
}

#[async_trait]
impl Runtime for KubernetesRuntime {
    async fn apply(&self, d: &DesiredState<'_>) -> Result<Observation, String> {
        if d.deleting {
            return self.delete_agent(d.name).await;
        }
        let name = object_name(d.name);
        if let Some(existing) = self.deployments.get_opt(&name).await.map_err(describe)? {
            if !self.t.owns(&existing.metadata, d.name) {
                return Err(self.not_ours("Deployment", &name, &existing.metadata));
            }
            if existing.metadata.deletion_timestamp.is_some() {
                // Queda una versión anterior terminando: se recrea al desaparecer.
                return Ok(Observation {
                    ready: 0,
                    converged: false,
                });
            }
            if is_tombstoned(&existing.metadata) {
                // Un borrado anterior quedó a medias y el agente se volvió a
                // crear: se quita la lápida para que la limpieza no lo borre.
                let unmark = serde_json::json!({"metadata": {"annotations": {A_DELETING: null}}});
                self.deployments
                    .patch(&name, &PatchParams::default(), &Patch::Merge(&unmark))
                    .await
                    .map_err(describe)?;
            }
        }
        // `force`: el orquestador es el dueño declarado de estos campos. Si
        // alguien cambia las réplicas con `kubectl scale` o un HPA, el
        // siguiente ciclo las devuelve a lo pedido por la CLI.
        let pp = PatchParams::apply(FIELD_MANAGER).force();
        let dep = self
            .deployments
            .patch(&name, &pp, &Patch::Apply(&self.t.deployment_manifest(d)))
            .await
            .map_err(describe)?;
        let uid = dep
            .metadata
            .uid
            .clone()
            .ok_or_else(|| "el API no devolvió el uid del Deployment".to_string())?;
        self.ensure_service(d, &uid).await?;
        observe_deployment(&dep, d.desired_replicas)
    }

    async fn recover(&self) -> Result<Vec<RecoveredAgent>, String> {
        let lp = ListParams::default().labels(&self.t.ownership_selector());
        let list = self.deployments.list(&lp).await.map_err(describe)?;
        let mut out = Vec::new();
        for dep in list.items {
            let meta = &dep.metadata;
            if meta.deletion_timestamp.is_some() || is_tombstoned(meta) {
                continue; // borrado en curso: lo termina la limpieza
            }
            let obj = meta.name.clone().unwrap_or_default();
            let Some(name) = meta.labels.as_ref().and_then(|l| l.get(L_AGENT)).cloned() else {
                continue;
            };
            let Some(model) = meta
                .annotations
                .as_ref()
                .and_then(|a| a.get(A_MODEL))
                .cloned()
            else {
                // Sin modelo no se puede reconstruir el agente. No se toca:
                // la limpieza solo borra lo marcado con la lápida.
                self.warn_once(
                    obj,
                    "Deployment gestionado sin anotación de modelo; no se recupera ni se borra",
                );
                continue;
            };
            let replicas = dep
                .spec
                .as_ref()
                .and_then(|s| s.replicas)
                .unwrap_or(0)
                .max(0) as u32;
            let clamped = replicas.min(MAX_REPLICAS);
            if clamped != replicas {
                tracing::warn!(object = %obj, replicas, max = MAX_REPLICAS, "réplicas fuera de rango; se recupera con el máximo");
            }
            out.push(RecoveredAgent {
                name,
                model,
                desired_replicas: clamped,
                created_at_unix: meta.creation_timestamp.as_ref().map(|t| t.0.timestamp()),
            });
        }
        Ok(out)
    }

    /// Termina los borrados marcados con la lápida que ya no están en el
    /// registro (por ejemplo, si el orquestador se reinició a mitad de un
    /// borrado). Nunca borra un Deployment sin lápida: si el orquestador no
    /// pudo recuperarlo, lo deja intacto y lo avisa.
    async fn garbage_collect(&self, known: &[String]) -> Result<usize, String> {
        let lp = ListParams::default().labels(&self.t.ownership_selector());
        let list = self.deployments.list(&lp).await.map_err(describe)?;
        let mut removed = 0;
        for dep in list.items {
            let meta = &dep.metadata;
            let Some(agent) = meta.labels.as_ref().and_then(|l| l.get(L_AGENT)) else {
                continue;
            };
            if known.iter().any(|k| k == agent) || meta.deletion_timestamp.is_some() {
                continue;
            }
            if !is_tombstoned(meta) {
                self.warn_once(
                    meta.name.clone().unwrap_or_default(),
                    "Deployment gestionado que no es ningún agente registrado; no se borra automáticamente",
                );
                continue;
            }
            // Un fallo con un objeto no impide seguir con los demás.
            match self.retire(&dep).await {
                Ok(()) => {
                    tracing::info!(agent = %agent, "borrado pendiente completado");
                    removed += 1;
                }
                Err(e) => {
                    tracing::warn!(agent = %agent, error = %e, "no se pudo completar el borrado")
                }
            }
        }
        Ok(removed)
    }

    fn kind(&self) -> &'static str {
        "kubernetes"
    }
}
