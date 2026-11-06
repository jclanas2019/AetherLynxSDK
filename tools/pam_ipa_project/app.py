"""API de autenticación y autorización de AetherLynx.

Configuración por variables de entorno:

  AETHERLYNX_JWT_SECRET            obligatorio, >= 32 bytes aleatorios
  AETHERLYNX_DB_PATH               ruta SQLite (por defecto: pam_ipa.db)
  AETHERLYNX_TOKEN_TTL_SECONDS     vigencia del token (por defecto: 600)
  AETHERLYNX_ALLOW_SELF_REGISTRATION  "true" para permitir /register público (por defecto: true)
  AETHERLYNX_DEFAULT_ROLE          rol que recibe un usuario auto-registrado (por defecto: user)
  AETHERLYNX_BOOTSTRAP_ADMIN_USER / AETHERLYNX_BOOTSTRAP_ADMIN_PASSWORD
                                   crean el primer administrador al arrancar, si no existe
  AETHERLYNX_RATE_LIMIT_PER_MINUTE peticiones por IP y minuto a /authenticate y /register (30)

Detrás de un proxy inverso, arranque uvicorn con --proxy-headers y
--forwarded-allow-ips para que el límite por IP vea la IP real del cliente.

Ejecutar:  uvicorn app:app
"""

from __future__ import annotations

import logging
import os
import threading
import time
from collections import defaultdict, deque
from contextlib import asynccontextmanager
from dataclasses import dataclass
from typing import Annotated

from fastapi import Depends, FastAPI, HTTPException, Request, status
from fastapi.responses import JSONResponse
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from pydantic import BaseModel, Field

import observability
import pam_ipa_project as pamlib
from opentelemetry import metrics, trace
from pam_ipa_project import PyAudit, PyAuthManager, PyIPA

log = logging.getLogger("aetherlynx.auth")

ADMIN_ROLE = "admin"


# --------------------------------------------------------------------------
# Configuración y estado
# --------------------------------------------------------------------------


@dataclass
class Settings:
    jwt_secret: str
    db_path: str
    token_ttl: int
    allow_self_registration: bool
    default_role: str
    bootstrap_user: str | None
    bootstrap_password: str | None
    rate_limit_per_minute: int

    @classmethod
    def from_env(cls) -> "Settings":
        secret = os.environ.get("AETHERLYNX_JWT_SECRET", "")
        if len(secret.encode()) < 32:
            raise RuntimeError(
                "AETHERLYNX_JWT_SECRET no está definido o tiene menos de 32 bytes. "
                "Genere uno con: python -c 'import secrets; print(secrets.token_urlsafe(48))'"
            )
        return cls(
            jwt_secret=secret,
            db_path=os.environ.get("AETHERLYNX_DB_PATH", "pam_ipa.db"),
            token_ttl=int(os.environ.get("AETHERLYNX_TOKEN_TTL_SECONDS", "600")),
            allow_self_registration=os.environ.get("AETHERLYNX_ALLOW_SELF_REGISTRATION", "true").lower()
            in ("1", "true", "yes"),
            default_role=os.environ.get("AETHERLYNX_DEFAULT_ROLE", "user"),
            bootstrap_user=os.environ.get("AETHERLYNX_BOOTSTRAP_ADMIN_USER") or None,
            bootstrap_password=os.environ.get("AETHERLYNX_BOOTSTRAP_ADMIN_PASSWORD") or None,
            rate_limit_per_minute=int(os.environ.get("AETHERLYNX_RATE_LIMIT_PER_MINUTE", "30")),
        )


class RateLimiter:
    """Ventana deslizante en memoria por (IP, ruta). Basta para un proceso;
    con varias réplicas, póngalo además en el ingress o el API gateway."""

    def __init__(self, per_minute: int) -> None:
        self.limit = per_minute
        self.hits: dict[tuple[str, str], deque] = defaultdict(deque)
        self.lock = threading.Lock()

    def allow(self, key: tuple[str, str]) -> bool:
        if self.limit <= 0:
            return True
        now = time.monotonic()
        with self.lock:
            q = self.hits[key]
            while q and now - q[0] > 60:
                q.popleft()
            if len(q) >= self.limit:
                return False
            q.append(now)
            if len(self.hits) > 50_000:  # evita crecer sin límite
                for k in [k for k, v in self.hits.items() if not v]:
                    del self.hits[k]
            return True


class State:
    settings: Settings
    auth: PyAuthManager
    ipa: PyIPA
    audit: PyAudit
    limiter: RateLimiter


state = State()


def _bootstrap_admin(s: Settings) -> None:
    """Crea el primer administrador. Solo otorga el rol a la cuenta que crea
    en este paso: si ya existe una cuenta con ese nombre, no la toca (podría
    haberla registrado cualquiera) ni vuelve a darle admin en cada arranque."""
    if not (s.bootstrap_user and s.bootstrap_password):
        return
    if state.auth.user_exists(s.bootstrap_user):
        name = s.bootstrap_user.strip().lower()
        if not state.ipa.has_role(name, ADMIN_ROLE):
            log.warning(
                "AETHERLYNX_BOOTSTRAP_ADMIN_USER='%s' ya existe sin rol admin; no se le otorga. "
                "Asígnelo con otro administrador si corresponde.",
                name,
            )
        return
    name = state.auth.register_user(s.bootstrap_user, s.bootstrap_password)
    state.ipa.add_role(name, ADMIN_ROLE)
    log.warning("administrador inicial '%s' creado; cambie su contraseña", name)


@asynccontextmanager
async def lifespan(_app: FastAPI):
    s = Settings.from_env()
    state.settings = s
    state.auth = PyAuthManager(s.db_path, s.jwt_secret, token_ttl_seconds=s.token_ttl)
    state.ipa = PyIPA(s.db_path)
    state.audit = PyAudit(s.db_path)
    state.limiter = RateLimiter(s.rate_limit_per_minute)
    _bootstrap_admin(s)
    telemetry = observability.setup("aetherlynx-auth", VERSION)
    try:
        yield
    finally:
        telemetry.shutdown()


VERSION = "0.4.0"
app = FastAPI(title="AetherLynx Auth", version=VERSION, lifespan=lifespan, **observability.fastapi_kwargs())
observability.instrument_fastapi(app, "aetherlynx.auth")

# Telemetría de negocio. Nunca lleva el nombre de usuario ni la IP: sólo el
# resultado, para que el colector pueda detectar picos de fallos (fuerza bruta).
_meter = metrics.get_meter("aetherlynx.auth")
LOGINS = _meter.create_counter(
    "aetherlynx.auth.logins", description="Inicios de sesión por resultado (success, failure, denied, rate_limited)"
)
REGISTRATIONS = _meter.create_counter("aetherlynx.auth.registrations", description="Altas de usuario por origen")
RATE_LIMITED = _meter.create_counter("aetherlynx.auth.rate_limited", description="Peticiones rechazadas por límite")


# --------------------------------------------------------------------------
# Errores: cada excepción del módulo Rust se traduce a un código HTTP concreto
# --------------------------------------------------------------------------

_ERRORS = {
    pamlib.InvalidInputError: (422, None),
    pamlib.UserExistsError: (status.HTTP_409_CONFLICT, "el usuario ya existe"),
    pamlib.InvalidCredentialsError: (status.HTTP_401_UNAUTHORIZED, "credenciales inválidas"),
    pamlib.InvalidTokenError: (status.HTTP_401_UNAUTHORIZED, "token inválido o expirado"),
    pamlib.NotFoundError: (status.HTTP_404_NOT_FOUND, "no encontrado"),
}


@app.exception_handler(pamlib.AuthError)
async def _auth_error_handler(_req: Request, exc: Exception) -> JSONResponse:
    for cls, (code, msg) in _ERRORS.items():
        if isinstance(exc, cls):
            headers = {"WWW-Authenticate": "Bearer"} if code == 401 else None
            return JSONResponse({"detail": msg or str(exc)}, status_code=code, headers=headers)
    # Errores internos: se registran completos pero no se exponen al cliente.
    log.exception("error interno de autenticación", exc_info=exc)
    return JSONResponse({"detail": "error interno"}, status_code=500)


# --------------------------------------------------------------------------
# Dependencias de seguridad
# --------------------------------------------------------------------------

_bearer = HTTPBearer(auto_error=False)


def current_user(
    creds: Annotated[HTTPAuthorizationCredentials | None, Depends(_bearer)],
) -> str:
    if creds is None or creds.scheme.lower() != "bearer":
        raise HTTPException(
            status.HTTP_401_UNAUTHORIZED, "falta el token de acceso", headers={"WWW-Authenticate": "Bearer"}
        )
    return state.auth.decode_token(creds.credentials)


CurrentUser = Annotated[str, Depends(current_user)]


def require_admin(user: CurrentUser) -> str:
    if not state.ipa.has_role(user, ADMIN_ROLE):
        raise HTTPException(status.HTTP_403_FORBIDDEN, "se requiere rol de administrador")
    return user


Admin = Annotated[str, Depends(require_admin)]


def rate_limited(request: Request) -> None:
    ip = request.client.host if request.client else "desconocida"
    if not state.limiter.allow((ip, request.url.path)):
        route = getattr(request.scope.get("route"), "path", request.url.path)
        RATE_LIMITED.add(1, {"route": route})
        if route == "/authenticate":
            LOGINS.add(1, {"result": "rate_limited"})
        raise HTTPException(
            status.HTTP_429_TOO_MANY_REQUESTS, "demasiadas peticiones; espere un minuto", headers={"Retry-After": "60"}
        )


RateLimited = Depends(rate_limited)


def _self_or_admin(user: str, target: str) -> None:
    if target.strip().lower() != user and not state.ipa.has_role(user, ADMIN_ROLE):
        raise HTTPException(status.HTTP_403_FORBIDDEN, "solo puede consultar su propia cuenta")


# --------------------------------------------------------------------------
# Modelos
# --------------------------------------------------------------------------

Username = Annotated[str, Field(min_length=3, max_length=64)]
Password = Annotated[str, Field(min_length=1, max_length=256)]
Label = Annotated[str, Field(min_length=1, max_length=64)]


class UserRegister(BaseModel):
    username: Username
    password: Password


class UserAuth(BaseModel):
    username: Username
    password: Password
    # Opcional: comprobar además el acceso a un recurso en el mismo paso.
    resource: Label | None = None


class TokenResponse(BaseModel):
    access_token: str
    token_type: str = "bearer"
    expires_in: int
    resource: str | None = None


class RoleAssign(BaseModel):
    role: Label


class ResourceCreate(BaseModel):
    resource: Label
    allowed_role: Label


class TokenBody(BaseModel):
    token: Annotated[str, Field(max_length=4096)]


class PasswordChange(BaseModel):
    old_password: Password
    new_password: Password


# --------------------------------------------------------------------------
# Rutas
# --------------------------------------------------------------------------


@app.get("/healthz", include_in_schema=False)
def healthz() -> dict:
    return {"status": "ok"}


@app.post("/register", status_code=status.HTTP_201_CREATED, dependencies=[RateLimited])
def register_user(body: UserRegister) -> dict:
    """Alta pública. El rol lo decide el servidor, nunca el cliente."""
    if not state.settings.allow_self_registration:
        raise HTTPException(status.HTTP_403_FORBIDDEN, "el auto-registro está desactivado")
    name = state.auth.register_user(body.username, body.password)
    state.ipa.add_role(name, state.settings.default_role)
    REGISTRATIONS.add(1, {"source": "self"})
    return {"message": "usuario registrado", "username": name, "roles": [state.settings.default_role]}


@app.post("/users", status_code=status.HTTP_201_CREATED)
def create_user(body: UserRegister, _admin: Admin) -> dict:
    """Alta por un administrador (sin rol; asígnelo con /assign-role)."""
    name = state.auth.register_user(body.username, body.password)
    REGISTRATIONS.add(1, {"source": "admin"})
    return {"message": "usuario creado", "username": name}


@app.post("/authenticate", response_model=TokenResponse, dependencies=[RateLimited])
def authenticate_user(body: UserAuth) -> TokenResponse:
    span = trace.get_current_span()
    try:
        token = state.auth.authenticate_user(body.username, body.password)
    except (pamlib.InvalidCredentialsError, pamlib.InvalidInputError):
        LOGINS.add(1, {"result": "failure"})
        span.set_attribute("aetherlynx.auth.result", "failure")
        raise
    if body.resource is not None:
        user = body.username.strip().lower()
        if not state.ipa.has_access(user, body.resource):
            LOGINS.add(1, {"result": "denied"})
            span.set_attribute("aetherlynx.auth.result", "denied")
            raise HTTPException(status.HTTP_403_FORBIDDEN, "acceso denegado al recurso")
    LOGINS.add(1, {"result": "success"})
    span.set_attribute("aetherlynx.auth.result", "success")
    return TokenResponse(access_token=token, expires_in=state.auth.token_ttl_seconds, resource=body.resource)


@app.get("/me")
def me(user: CurrentUser) -> dict:
    return {"username": user, "roles": state.ipa.get_roles(user)}


@app.post("/me/password")
def change_password(body: PasswordChange, user: CurrentUser) -> dict:
    """Cambia la contraseña e invalida todos los tokens anteriores."""
    state.auth.change_password(user, body.old_password, body.new_password)
    return {"message": "contraseña actualizada; vuelva a iniciar sesión"}


@app.post("/me/logout-all")
def logout_all(user: CurrentUser) -> dict:
    """Invalida todos los tokens de la cuenta, incluido el de esta petición."""
    state.auth.revoke_tokens(user)
    return {"message": "sesiones cerradas"}


@app.get("/users/{username}")
def get_user(username: str, user: CurrentUser) -> dict:
    _self_or_admin(user, username)
    if not state.auth.user_exists(username):
        raise HTTPException(status.HTTP_404_NOT_FOUND, "usuario no encontrado")
    return {"username": username.strip().lower(), "roles": state.ipa.get_roles(username)}


@app.post("/assign-role/{username}")
def assign_role(username: str, body: RoleAssign, _admin: Admin) -> dict:
    state.ipa.add_role(username, body.role)
    return {"message": "rol asignado"}


@app.delete("/assign-role/{username}/{role}")
def revoke_role(username: str, role: str, admin: Admin) -> dict:
    if username.strip().lower() == admin and role.strip().lower() == ADMIN_ROLE:
        raise HTTPException(status.HTTP_409_CONFLICT, "no puede quitarse a sí mismo el rol de administrador")
    state.ipa.remove_role(username, role)
    return {"message": "rol revocado"}


@app.post("/add-resource", status_code=status.HTTP_201_CREATED)
def add_resource(body: ResourceCreate, _admin: Admin) -> dict:
    state.ipa.add_resource(body.resource, body.allowed_role)
    return {"message": f"recurso '{body.resource}' disponible para el rol '{body.allowed_role}'"}


@app.get("/accessible-resources/{username}")
def list_accessible_resources(username: str, user: CurrentUser) -> dict:
    _self_or_admin(user, username)
    return {"username": username.strip().lower(), "accessible_resources": state.ipa.list_accessible_resources(username)}


@app.post("/verify-token")
def verify_token(body: TokenBody) -> dict:
    try:
        sub = state.auth.decode_token(body.token)
    except pamlib.InvalidTokenError:
        return {"is_valid": False}
    return {"is_valid": True, "sub": sub}


@app.get("/audit")
def audit_events(_admin: Admin, limit: int = 100) -> dict:
    return {"events": state.audit.events(max(1, min(limit, 1000)))}
