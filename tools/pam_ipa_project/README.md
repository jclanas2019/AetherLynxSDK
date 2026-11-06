# AetherLynx SDK · Auth (PAM/IPA)

![Aether Lynx PAMPIM](Aether%20LynxPAMPIM.png)

Autenticación y autorización por roles para AetherLynx. El núcleo está en Rust y se usa desde Python; `app.py` lo expone como API FastAPI.

- **PAM (autenticación):** contraseñas con Argon2id y sal por usuario, y tokens JWT HS256 con emisor, audiencia, expiración e identificador únicos. Cada token queda ligado a la cuenta concreta y a su versión de credenciales: cambiar la contraseña o cerrar todas las sesiones invalida los tokens anteriores, y un token no sirve para otra cuenta creada después con el mismo nombre.
- **Protección contra fuerza bruta:** bloqueo temporal tras 5 intentos fallidos (el intento se cuenta de forma atómica *antes* de verificar, así que peticiones en paralelo no pueden superar el límite) y límite de 30 peticiones por minuto e IP en `/authenticate` y `/register`. Login con usuario inexistente, contraseña errónea o cuenta bloqueada responde igual y tarda lo mismo. El bloqueo por cuenta permite que un tercero bloquee una cuenta concreta durante 15 minutos; el límite por IP lo encarece, y en producción conviene añadir limitación también en el ingress.
- **IPA (autorización):** roles por usuario y roles permitidos por recurso, persistidos en la misma base SQLite. Se deniega por defecto.
- **Auditoría:** cada registro, login y cambio de contraseña queda en la tabla `audit_log`.

## Estructura

| Archivo | Contenido |
|---|---|
| `src/pam.rs` | Registro, login, bloqueo, emisión y validación de tokens |
| `src/ipa.rs` | Roles y recursos |
| `src/db.rs` | Esquema y acceso a SQLite |
| `src/validate.rs` | Formato de usuarios, contraseñas, roles y recursos |
| `src/lib.rs` | Bindings de Python (`PyAuthManager`, `PyIPA`, `PyAudit`) |
| `app.py` | API FastAPI |
| `cliente.py` | Cliente interactivo de prueba |

## Compilar

```bash
pip install maturin
maturin develop --release        # o: maturin build --release && pip install target/wheels/*.whl
pip install -r requirements-api.txt
```

## Configurar y ejecutar

```bash
export AETHERLYNX_JWT_SECRET="$(python3 -c 'import secrets; print(secrets.token_urlsafe(48))')"
export AETHERLYNX_DB_PATH=pam_ipa.db
export AETHERLYNX_BOOTSTRAP_ADMIN_USER=admin
export AETHERLYNX_BOOTSTRAP_ADMIN_PASSWORD='una contraseña larga'   # cámbiela después
uvicorn app:app
```

| Variable | Por defecto | Descripción |
|---|---|---|
| `AETHERLYNX_JWT_SECRET` | obligatoria | Al menos 32 bytes aleatorios. Sin ella la API no arranca. |
| `AETHERLYNX_DB_PATH` | `pam_ipa.db` | Base SQLite compartida por auth, roles y auditoría |
| `AETHERLYNX_TOKEN_TTL_SECONDS` | `600` | Vigencia del token |
| `AETHERLYNX_ALLOW_SELF_REGISTRATION` | `true` | Permite `/register` público |
| `AETHERLYNX_DEFAULT_ROLE` | `user` | Rol de quien se auto-registra |
| `AETHERLYNX_BOOTSTRAP_ADMIN_USER` / `_PASSWORD` | sin definir | Crea el primer administrador. Si ya existe una cuenta con ese nombre, no la toca ni le da admin. |
| `AETHERLYNX_RATE_LIMIT_PER_MINUTE` | `30` | Peticiones por IP y minuto a login y registro (0 lo desactiva). Detrás de un proxy, use `uvicorn --proxy-headers --forwarded-allow-ips=...` |

## Endpoints

| Método y ruta | Acceso | Descripción |
|---|---|---|
| `POST /register` | público | Alta con el rol por defecto. El cliente no elige el rol. Responde 409 si el nombre ya existe: como en cualquier registro abierto, eso revela qué nombres están tomados; desactive el auto-registro si no es aceptable. |
| `POST /authenticate` | público | Devuelve `access_token`. Con `resource`, comprueba además el acceso (403 si no lo tiene). |
| `GET /me` | token | Usuario y roles propios |
| `POST /me/password` | token | Cambio de contraseña; invalida los tokens anteriores |
| `POST /me/logout-all` | token | Cierra todas las sesiones de la cuenta |
| `GET /users/{u}`, `GET /accessible-resources/{u}` | el propio usuario o admin | |
| `POST /users` | admin | Alta de usuario |
| `POST /assign-role/{u}`, `DELETE /assign-role/{u}/{rol}` | admin | Gestión de roles |
| `POST /add-resource` | admin | `{"resource", "allowed_role"}` |
| `GET /audit` | admin | Últimos eventos de auditoría |
| `POST /verify-token` | público | `{"token"}` → `{"is_valid", "sub"}` |

## Pruebas

```bash
cargo test                 # núcleo en Rust
pip install pytest httpx
pytest -q tests            # API completa contra el módulo compilado
```

## Migración desde la versión 0.1

La 0.1 guardaba los roles solo en memoria y firmaba los tokens con una clave fija. Al abrir una base 0.1, la 0.2 la migra sola: renombra la columna de contraseña, añade las columnas nuevas y pasa los nombres a minúsculas. Si dos cuentas solo difieren en mayúsculas (`Bob` y `BOB`), las deja intactas, avisa por la salida de error y no podrán iniciar sesión hasta que un administrador las renombre. Los hashes de la 0.1 eran Argon2, así que las contraseñas siguen funcionando. Después:

1. Defina `AETHERLYNX_JWT_SECRET`. Todos los tokens anteriores dejan de ser válidos, que es lo deseado.
2. Vuelva a asignar los roles con un administrador: en la 0.1 nunca se guardaron.
3. Pida a los usuarios que cambien su contraseña. La 0.1 permitía sobrescribir la de otra persona registrándose con su nombre, así que no hay garantía de que cada hash sea del dueño legítimo.
4. Haga una copia de la base antes de migrar.
