# AetherLynx SDK

![AetherLynx SDK Logo](AetherLynxLogo.png)

AetherLynx SDK orquesta agentes de modelos de lenguaje (LLM) con un enfoque declarativo inspirado en Kubernetes: usted declara cuántas réplicas quiere de cada agente y un reconciliador hace converger el estado real hacia ese objetivo.

## Componentes

| Componente | Ruta | Qué hace |
|---|---|---|
| Orquestador | `agent_system/` | Servidor gRPC + reconciliador. Guarda el estado deseado de cada agente y pide al *runtime* que converja. |
| CLI | `cli/` | `aetherlynx-cli`: crea, escala, lista y elimina agentes; consulta el estado. |
| API compartida | `proto/aetherlynx.proto` | Contrato gRPC único para servidor y CLI. |
| Agente de consultas | `agents/` | Agente de ejemplo que responde preguntas de estudiantes con datos académicos y financieros minimizados. |
| Auth (PAM/IPA) | `tools/pam_ipa_project/` | Autenticación con Argon2id + JWT y autorización por roles, en Rust con API FastAPI. |
| Vault | `tools/vault/` | Almacén de secretos AES-256-GCM con clave derivada por Argon2id. |
| Despliegue | `kubernetes/`, `scripts/` | Manifiestos endurecidos y script de despliegue. |

> **Runtime.** El orquestador incluye un runtime *simulado* que mantiene las réplicas en memoria y las sube o baja gradualmente. Sirve para desarrollo y pruebas. Para producción, implemente el trait `Runtime` (`agent_system/src/runtime.rs`) contra su planificador, por ejemplo escalando un Deployment de Kubernetes por agente.

## Requisitos

- Rust **1.88** o superior y `protoc` (`apt install protobuf-compiler` / `brew install protobuf`)
- Python **3.9** o superior; `maturin` para compilar los módulos de `tools/`

## Inicio rápido

```bash
git clone https://github.com/jclanas2019/AetherLynxSDK.git
cd AetherLynxSDK
cargo build --release

# El orquestador exige un token de al menos 32 caracteres
export AETHERLYNX_API_TOKEN="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
./target/release/aetherlynx-orchestrator start
```

En otra terminal, con el mismo `AETHERLYNX_API_TOKEN`:

```bash
aetherlynx-cli agent create --name student-agent --model gpt-4o --replicas 3
aetherlynx-cli agent scale  --name student-agent --replicas 5
aetherlynx-cli agent list
aetherlynx-cli orchestrator status
aetherlynx-cli agent delete --name student-agent
aetherlynx-cli agent list -o json      # salida para scripts
```

Opciones útiles del orquestador (`aetherlynx-orchestrator start --help`):

| Opción | Variable | Por defecto |
|---|---|---|
| `--listen` | `AETHERLYNX_LISTEN` | `127.0.0.1:50051` |
| `--api-token` | `AETHERLYNX_API_TOKEN` | obligatorio |
| `--tls-cert`, `--tls-key` | `AETHERLYNX_TLS_CERT`, `AETHERLYNX_TLS_KEY` | sin TLS |
| `--reconcile-interval` | | `5s` |
| `--integration-url` | `AETHERLYNX_INTEGRATION_URL` | desactivada |
| `--log-format text\|json` | `AETHERLYNX_LOG_FORMAT` | `text` |

La CLI lee `AETHERLYNX_ENDPOINT` (por defecto `http://127.0.0.1:50051`), `AETHERLYNX_API_TOKEN` y, para TLS con CA propia, `AETHERLYNX_CA_CERT`.

## Modelo de seguridad

- **Autenticación obligatoria.** Sin token el orquestador no arranca. `--allow-unauthenticated` existe solo para desarrollo y solo en loopback.
- **Sin texto plano en la red.** Escuchar fuera de loopback exige TLS (`--tls-cert`/`--tls-key`), salvo que se indique `--allow-insecure-transport` porque un service mesh ya cifra el tráfico.
- **Entradas validadas.** Nombres de agente con formato DNS-1123, modelos con caracteres acotados y réplicas entre 0 y 100. Nada sin validar llega al estado ni a los logs.
- **Fallar en voz alta.** Si el servidor gRPC no puede arrancar, el proceso termina con error.
- **Datos personales.** El agente de consultas solo envía al LLM una lista explícita de campos no identificativos. Vea `agents/student_queries_agent.py`.

## Herramientas

- [Auth PAM/IPA](tools/pam_ipa_project/README.md)
- [Vault](tools/vault/README.md)

## Agente de consultas

```bash
cd agents
pip install -r requirements.txt
export UNIVERSITY_API_URL=https://api.university.edu UNIVERSITY_API_TOKEN=... OPENAI_API_KEY=...
python student_queries_agent.py 12345 "¿Cuándo puedo inscribir ramos para el próximo semestre?"

# como servicio HTTP (el modo que usa Kubernetes)
export AETHERLYNX_AGENT_TOKEN="$(python3 -c 'import secrets; print(secrets.token_urlsafe(48))')"
uvicorn service:app --port 5000
```

## Despliegue en Kubernetes

```bash
docker build -t <registro>/aetherlynx/university-agent:0.2.0 agents/
docker build -f Dockerfile.orchestrator -t <registro>/aetherlynx/orchestrator:0.2.0 .
# cree el secreto según kubernetes/examples/agent-secret.example.yaml y ajuste la imagen en agent-deployment.yaml
scripts/deploy_k8s.sh --dry-run
scripts/deploy_k8s.sh
```

Los pods corren sin root, con sistema de archivos de solo lectura, sin capacidades, con límites de recursos y sondas de salud. La NetworkPolicy admite entrada solo desde pods etiquetados `aetherlynx.io/agent-client: "true"` y salida solo a DNS y a HTTPS fuera del clúster; si el API de la universidad es interno, añada su CIDR.

La imagen del orquestador escucha en `0.0.0.0:50051` y, por diseño, no arranca sin `AETHERLYNX_API_TOKEN` ni sin TLS. Pásele `AETHERLYNX_TLS_CERT`/`AETHERLYNX_TLS_KEY` (por ejemplo, montando un secreto `kubernetes.io/tls`), o `--allow-insecure-transport` si un service mesh ya cifra el tráfico con mTLS.

## Pruebas

```bash
cargo test --workspace                         # orquestador y CLI
(cd tools/vault && cargo test)                 # núcleo del vault
(cd tools/pam_ipa_project && cargo test)       # núcleo de auth
# módulos Python: compile con maturin y ejecute pytest en cada carpeta tests/
(cd agents && pytest -q tests)
```

La integración continua (`.github/workflows/ci.yml`) ejecuta además `cargo fmt`, `cargo clippy -D warnings`, `cargo audit --deny warnings` sobre los tres lockfiles y `pip-audit` sobre las dependencias Python. Dependabot (`.github/dependabot.yml`) propone actualizaciones semanales.

## Contribuir

Haga un fork, cree una rama y envíe un pull request. Antes, ejecute las pruebas y `cargo fmt`.

Juan Carlos Lanas Ocampo · jc@lab-ai.org

## Licencia

Apache License 2.0. Vea [LICENSE](LICENSE).
