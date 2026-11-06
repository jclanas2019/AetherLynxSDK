# Changelog

## 0.4.0 · Observabilidad operativa

Telemetría de punta a punta con **OpenTelemetry** y un colector y panel propios: ningún servicio ni aplicación de observabilidad de terceros (sin Prometheus, Grafana, Jaeger ni CDN). Solo se usan las bibliotecas oficiales del SDK de OpenTelemetry.

### Colector `aetherlynx-telemetry` (nuevo crate `telemetry/`)

- **Nuevo · Ingesta OTLP/HTTP** (protobuf, con o sin gzip) en `:4318`, en las rutas estándar `/v1/traces`, `/v1/metrics` y `/v1/logs`. Cualquier SDK de OpenTelemetry puede enviarle datos sin configuración especial.
- **Nuevo · Almacenamiento SQLite** (WAL) con retención configurable (24 h por defecto) y limpieza incremental.
- **Nuevo · Alertas evaluadas en el colector:** `ServiceSilent`, `ReconcileStalled`, `HighErrorRate`, `AgentsFailed`, `ReplicasBelowDesired`, `LoginFailureSpike`, `LlmErrors` y `UpstreamErrors`. Cada una tiene su pista y su procedimiento en `docs/RUNBOOK.md`.
- **Nuevo · Panel web** en `:8088`: resumen con alertas, servicios y gráficas; listado de operaciones; trazas en cascada; métricas y logs. Sin dependencias ni recursos externos, en tema claro y oscuro, y usable en el móvil.
- **Nuevo · API de consulta** (`/api/...`), la misma que usa el panel.
- **Seguridad.** Token de ingesta y token de consulta separados, de al menos 32 caracteres. Exige TLS fuera de loopback. Límites de cuerpo (8 MiB), de descompresión (32 MiB, contra bombas gzip), de atributos y de longitud de cadenas. CSP estricta sin estilos ni scripts en línea.
- **Protección ante clientes que envían de más.** Hasta 64 conexiones por puerto, cabeceras en 10 s y 8 lotes procesados a la vez. Las marcas de tiempo futuras se traen al presente, para que no escapen de la retención. Máximo de servicios distintos (`--max-services`) y tamaño máximo de la base (`--max-db-mib`), que al llenarse descarta lo más antiguo. Los histogramas mal formados se descartan al recibirlos. Las ordenaciones de SQLite van en memoria, porque el contenedor no tiene disco temporal escribible.
- **Operaciones = peticiones atendidas.** Una operación es un span de servidor que es la raíz de su traza o continúa una traza llegada de otro proceso. Así un servicio llamado por otro (o por un cliente que no exporta) tiene sus propias estadísticas, y el trabajo interno (los ciclos del reconciliador) no diluye la tasa de error ni las latencias.
- **`ReconcileStalled` mide progreso, no solo actividad.** Un ciclo en que fallan todas las llamadas al runtime ya no cuenta como éxito, y la alerta también salta si el orquestador nunca completa un ciclo con progreso desde que arranca.
- **CLI.** Los datos recibidos se muestran con los caracteres de control neutralizados, para que nada enviado al colector pueda manipular la terminal. La CLI avisa si envía el token de consulta sin TLS a un host remoto.

### Instrumentación

- **Orquestador.** Spans de cada petición gRPC (continuando el `traceparent` entrante) y de cada ciclo de reconciliación, con `runtime.apply/delete/garbage_collect/recover` como hijos. Métricas de ciclos, duración, último éxito, agentes por fase, réplicas deseadas y listas, operaciones del runtime y peticiones gRPC. Logs correlacionados con la traza. Solo los errores del servidor marcan un span como fallido; los del cliente no.
- **CLI.** Envía `traceparent` en cada llamada. `--show-trace` imprime el identificador. Nuevo subcomando `telemetry` con `status`, `alerts` (código de salida 2 si hay alguna crítica), `services`, `traces`, `trace <id>` (cascada en ASCII) y `logs`.
- **Agente de consultas.** Spans `POST /query` → `agent.answer` → `upstream.*` y `llm.complete` (con los atributos `gen_ai.*` de OpenTelemetry). Métricas de consultas por resultado, llamadas y latencia del LLM (incluidos los tokens) y del API de la universidad. Propaga la traza al API de la universidad.
- **API de auth.** Un span por petición con la plantilla de la ruta. Métricas `aetherlynx.auth.logins{result=success|failure|denied|rate_limited}`, `aetherlynx.auth.registrations{source}` y `aetherlynx.auth.rate_limited`.
- **Módulo común `observability.py`** para los dos servicios Python. Se configura con las variables estándar `OTEL_*`. Sin `OTEL_EXPORTER_OTLP_ENDPOINT` no hace nada. Un test comprueba que las dos copias son idénticas.
- **Privacidad.** La telemetría no lleva identificadores de estudiantes, preguntas, respuestas, nombres de usuario, contraseñas ni tokens. Las rutas se registran como plantilla. Se desactiva la telemetría nativa de FastAPI 0.16x, que duplicaba los spans y cuyos logs de validación incluyen los valores recibidos. Hay tests que buscan esos datos en spans, métricas y logs, y una prueba de punta a punta con los tres servicios que los buscó en la base del colector.

### Despliegue

- **Nuevo · `kubernetes/telemetry.yaml`:** el colector con una réplica, volumen persistente, TLS, sin salida de red y NetworkPolicy de entrada. También el ConfigMap `aetherlynx-telemetry-client` para los clientes. Plantilla de secretos en `kubernetes/examples/telemetry-secret.example.yaml`. Nueva imagen `Dockerfile.telemetry` y nueva opción `scripts/deploy_k8s.sh --telemetry`.
- **Cambio · Pods de agente y orquestador.** Reciben la configuración, la cabecera de autenticación (desde un Secret, nunca en claro en el manifiesto) y la CA del colector. Las tres son opcionales: sin el colector, los pods arrancan igual. Nueva opción del orquestador `--agent-telemetry` (prefijo de esos objetos; `""` para no usarlos).
- **Cambio · NetworkPolicies.** Agentes y orquestador pueden salir hacia el colector (puerto 4318) dentro del namespace.
- **Cambio · CA ausente.** Si `OTEL_EXPORTER_OTLP_CERTIFICATE` apunta a un archivo que no existe, el servicio arranca sin exportar y lo avisa una vez, en vez de fallar o de llenar el log de errores.
- **Pruebas.** Un nuevo test deserializa todos los manifiestos con los tipos de Kubernetes y comprueba que los nombres de los objetos de telemetría coinciden con los que usa el runtime. `defaultMode` se escribe en decimal, para que ningún parser YAML lo interprete distinto.
- **Revisión independiente.** Antes de publicar, una revisión independiente encontró nueve defectos. Entre ellos, `ReplicasBelowDesired` nunca saltaba con el SDK real, porque cada instrumento sella su propia hora. Un histograma mal formado podía romper una gráfica, y timestamps futuros podían llenar el disco. Todos están corregidos y cubiertos por tests.

## 0.3.0 · Orquestador y CLI

### Runtime de Kubernetes

- **Nuevo · `--runtime kubernetes`.** Cada agente es un Deployment y un Service `aetherlynx-<agente>`, aplicados con server-side apply. El Service tiene al Deployment como propietario y solo se aplica una vez por Deployment.
- **Nuevo · Propiedad estricta.** El runtime nunca modifica ni borra un objeto sin las etiquetas de su instancia, aunque tenga el nombre esperado, y los borrados llevan precondición de `uid`.
- **Nuevo · Estado deseado durable.** Al arrancar, el orquestador recupera sus agentes desde los Deployments del clúster. Los borrados se marcan antes con una lápida (`aetherlynx.io/deleting`), así que un borrado interrumpido por un reinicio se completa después. La limpieza solo borra lo marcado; lo que no puede recuperar lo deja intacto y lo avisa.
- **Nuevo · «Ready» con rollout real.** Un agente solo está listo con la generación observada y todas las réplicas actualizadas y listas. Los rollouts atascados y los errores de cuota o RBAC se muestran como `Failed` con un mensaje legible; las condiciones de un rollout anterior no cuentan.
- **Nuevo · `render-agent`.** Imprime el Deployment y el Service de un agente sin tocar el clúster, apto para `kubectl diff`.
- **Nuevo · Despliegue del orquestador** (`kubernetes/orchestrator.yaml`): ServiceAccount, Role limitado a Deployments y Services del namespace, una réplica con `Recreate`, TLS legible por el proceso (`fsGroup`) y NetworkPolicy. `scripts/deploy_k8s.sh --orchestrator`.
- **Cambio · Trait `Runtime` asíncrono**, con observaciones (`ready`, `converged`), recuperación y limpieza. Hasta 8 agentes se reconcilian a la vez y cada llamada tiene un tiempo máximo: un API lento marca el agente como `Failed` en vez de congelar el bucle.
- **Cambio · Nombres de agente hasta 52 caracteres** (antes 63), para que `aetherlynx-<nombre>` sea un nombre válido de Service.
- **Cambio · `orchestrator status`** muestra el runtime en uso.
- **Seguridad.** Los pods generados cumplen el perfil Pod Security «restricted». Sin `blockOwnerDeletion`, que exigiría permisos extra en clústeres como OpenShift. El cliente respeta `HTTPS_PROXY`/`NO_PROXY` (sin la feature `http-proxy`, `kube` se negaba a arrancar en entornos con proxy). `cargo audit --deny warnings` sigue limpio; la única excepción, `RUSTSEC-2024-0449` (`unsafe-libyaml`, sin mantenimiento, que `kube-client` usa para leer el kubeconfig, incluso en kube 3.x), está justificada en `.cargo/audit.toml`.
- **Pruebas.** El runtime se prueba con el cliente `kube` real contra un API server simulado a nivel HTTP. El simulador hace cumplir la inmutabilidad del selector, las precondiciones de borrado y la semántica de los parches, y puede simular borrados fallidos. Se verificó con siete mutaciones del código que los tests detectan errores en apply, convergencia, borrado, propiedad, lápida y limpieza. Una revisión independiente del runtime encontró diez defectos antes de publicar (entre ellos, borrar objetos ajenos y borrar agentes no recuperados); todos están corregidos y cubiertos por tests.

## 0.2.0

Versión de corrección tras la auditoría de seguridad y calidad. Cada entrada indica el hallazgo que resuelve. Las marcadas «Revisión» salieron de una segunda revisión independiente del código ya corregido.

### Seguridad

- **C-1 · Secreto JWT fijo.** Se eliminó `b"secret"`. El secreto llega por `AETHERLYNX_JWT_SECRET` (mínimo 32 bytes) y la API no arranca sin él. Los tokens llevan y se validan con `iss`, `aud`, `exp`, `nbf` y `jti`, más el identificador de la cuenta y su versión de credenciales: cambiar la contraseña o usar `/me/logout-all` invalida los tokens anteriores, y un token no pasa a otra cuenta creada después con el mismo nombre.
- **C-2 · API de gestión abierta.** Todas las rutas de gestión exigen token y rol `admin`. `/register` ya no deja elegir el rol. Un usuario solo puede consultar su propia cuenta.
- **Revisión · Escalada vía administrador inicial.** El arranque solo da `admin` a la cuenta que él mismo crea. Ya no promueve una cuenta preexistente con ese nombre ni deshace en cada reinicio una revocación de admin.
- **Nuevo · Toma de cuentas al re-registrar.** `INSERT OR REPLACE` permitía sobrescribir la contraseña de otro usuario registrándose con su nombre. Ahora el alta falla con 409 y la contraseña original se conserva.
- **H-1 · Clave del vault perdida al reiniciar.** La clave se deriva de una passphrase con Argon2id; sal y parámetros persisten en `vault_meta`. Se añadieron detección de passphrase incorrecta, AAD por nombre de secreto y rotación de passphrase.
- **Revisión · Pérdida de datos al rotar.** Un orden de bloqueo único (conexión → clave) se mantiene durante el cifrado y la escritura, y otros handles abiertos detectan la rotación (`StaleVaultError`) en vez de escribir con la clave vieja. Los parámetros de Argon2id se acotan (mínimo OWASP, máximo 4 GiB) también al leerlos de la base.
- **H-2 · Base de secretos versionada.** Se eliminó `tools/vault/my_vault.db` y `.gitignore` excluye `*.db`.
- **H-3 · gRPC sin TLS ni autenticación.** Token bearer obligatorio con comparación de tiempo constante; TLS con `--tls-cert/--tls-key`; se impide escuchar fuera de loopback sin TLS salvo que se pida explícitamente.
- **H-4 · Roles en memoria.** Roles y recursos persisten en SQLite junto a los usuarios.
- **H-5 · Dependencias obsoletas.** `pyo3` 0.18 → 0.30 (corrige RUSTSEC-2025-0020, RUSTSEC-2026-0176 y RUSTSEC-2026-0177), `ring` 0.16 → 0.17, `base64` 0.13 → 0.22, `jsonwebtoken` 8 → 9, `rusqlite` → 0.32, `rcgen` 0.10 → 0.13, `tonic` 0.9/0.10 → 0.14 unificado (retira `rustls-pemfile`, sin mantenimiento). Se retiró `langchain`; el agente usa el SDK oficial de OpenAI con rangos acotados. `cargo audit --deny warnings` pasa limpio en los tres lockfiles y `pip-audit` no encuentra vulnerabilidades.
- **M-2 · Inyección en logs.** Entradas validadas antes de registrarse; logs estructurados con `tracing` (texto o JSON).
- **M-5 · Validación débil de tokens.** Ver C-1.
- **M-6 · PII al LLM.** El agente solo envía una lista explícita de campos no identificativos, trata el contexto como datos y no reenvía errores internos al prompt.
- **Nuevo · Enumeración en el login.** Usuario inexistente, contraseña errónea y cuenta bloqueada devuelven el mismo error y tardan lo mismo. (`/register` sí revela si un nombre está tomado, como cualquier registro abierto; se puede desactivar.)
- **Nuevo · Fuerza bruta.** Bloqueo temporal tras 5 intentos fallidos y límite de peticiones por IP en login y registro.
- **Revisión · Bloqueo esquivable en paralelo.** El intento se reserva de forma atómica antes de verificar la contraseña, así que peticiones simultáneas no superan el límite; un bloqueo vencido reinicia el contador en vez de volver a bloquear al primer fallo.

### Correcciones

- **M-1 · Funciones de fachada.** El orquestador mantiene un registro real de agentes y un reconciliador que converge réplicas a través del trait `Runtime`. La CLI implementa `create`, `scale`, `get`, `list`, `delete` y `orchestrator status`, con salida en tabla o JSON y códigos de salida correctos.
- **M-3 · Pánicos.** Se eliminaron los `unwrap()`/`expect()` en rutas alcanzables; un hash corrupto cuenta como login fallido.
- **M-4 · Pánico al arrancar en debug.** Se quitó el subcomando `help` duplicado; la CLI usa `clap` derive.
- **M-7 · Kubernetes.** Manifiestos con usuario no root, sistema de archivos de solo lectura, sin capacidades, límites, sondas, Service, PodDisruptionBudget, NetworkPolicy y ServiceAccount sin token. Validados contra el esquema de Kubernetes 1.30. La salida HTTPS excluye rangos internos y el endpoint de metadatos; la plantilla del secreto vive en `kubernetes/examples/` y el servicio rechaza su valor de ejemplo. El script de despliegue usa `set -euo pipefail` y solo referencia archivos existentes; la referencia a `prometheus-config.yaml`, que nunca existió, se retiró. Dockerfiles para el agente y el orquestador.
- **Nuevo · Fallo silencioso del servidor.** Si el gRPC no puede arrancar (puerto ocupado, etc.), el proceso termina con error en vez de seguir vivo sin API.
- **Nuevo · Sondeo de datos personales.** El reconciliador ya no pide los datos del estudiante 12345 cada 10 s; la integración es una sonda de salud opcional y configurable.
- **Nuevo · Certificados sin clave.** `generate_certificate` devuelve también la clave privada (antes se descartaba) y ya no falla el 29 de febrero.
- **L-1 · Versiones gRPC distintas.** Un solo `.proto` y un workspace de Cargo para servidor y CLI.
- **Revisión · Migración de la 0.1.** Se migra sola (columnas, nombres en minúsculas, identificador de cuenta) y ya no aborta si dos usuarios solo difieren en mayúsculas.
- **Revisión · CLI.** La detección de loopback compara el host parseado (`localhost.evil.com` ya no cuenta), `--ca-cert` con `http://` es un error y el token se marca como sensible.
- **L-2 · Código muerto.** Se eliminaron `auth.rs`, `mod.rs` y los `Readme.me` vacíos.
- **L-3 · README desincronizado.** Los comandos documentados se probaron contra la CLI.
- **L-4 · Licencia.** Apache-2.0 en README y en todos los `Cargo.toml`, igual que `LICENSE`.
- **L-5 · HTTP sin timeouts.** Timeouts y reintentos acotados en todas las llamadas salientes.
- **L-7 · Sin pruebas.** 102 pruebas (58 en Rust y 44 en Python) y CI con fmt, clippy, tests en debug, auditoría de dependencias y Dependabot.
