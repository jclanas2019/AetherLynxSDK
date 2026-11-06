# Changelog

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
