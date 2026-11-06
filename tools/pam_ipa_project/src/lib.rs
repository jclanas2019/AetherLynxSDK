//! Bindings de Python para la autenticación (`PyAuthManager`) y la
//! autorización por roles (`PyIPA`) de AetherLynx.

use std::sync::Arc;

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyDict;

pub mod db;
pub mod ipa;
pub mod pam;
pub mod validate;

use pam::{AuthConfig, AuthError, Authenticator};

create_exception!(
    pam_ipa_project,
    AuthError_,
    PyException,
    "Error base del módulo."
);
create_exception!(
    pam_ipa_project,
    InvalidInputError,
    AuthError_,
    "Entrada con formato inválido."
);
create_exception!(
    pam_ipa_project,
    UserExistsError,
    AuthError_,
    "El usuario ya existe."
);
create_exception!(
    pam_ipa_project,
    InvalidCredentialsError,
    AuthError_,
    "Credenciales inválidas."
);
create_exception!(
    pam_ipa_project,
    InvalidTokenError,
    AuthError_,
    "Token inválido o expirado."
);
create_exception!(
    pam_ipa_project,
    NotFoundError,
    AuthError_,
    "Recurso o usuario no encontrado."
);

fn to_py(e: AuthError) -> PyErr {
    let msg = e.to_string();
    match e {
        AuthError::Invalid(_) => InvalidInputError::new_err(msg),
        AuthError::UserExists => UserExistsError::new_err(msg),
        AuthError::InvalidCredentials => InvalidCredentialsError::new_err(msg),
        AuthError::InvalidToken => InvalidTokenError::new_err(msg),
        AuthError::NotFound => NotFoundError::new_err(msg),
        AuthError::Internal(_) => AuthError_::new_err(msg),
    }
}

fn open_store(db_path: &str) -> PyResult<Arc<db::Store>> {
    db::Store::open(db_path)
        .map(Arc::new)
        .map_err(|e| AuthError_::new_err(format!("no se pudo abrir la base de datos: {e}")))
}

#[pyclass(frozen)]
pub struct PyAuthManager {
    inner: Arc<Authenticator>,
}

#[pymethods]
impl PyAuthManager {
    /// PyAuthManager(db_path, jwt_secret, *, issuer, audience, token_ttl_seconds,
    /// max_failed_attempts, lockout_seconds)
    #[new]
    #[pyo3(signature = (
        db_path, jwt_secret, *, issuer = "aetherlynx", audience = "aetherlynx-api",
        token_ttl_seconds = 600, max_failed_attempts = 5, lockout_seconds = 900
    ))]
    fn new(
        db_path: &str,
        jwt_secret: &str,
        issuer: &str,
        audience: &str,
        token_ttl_seconds: u64,
        max_failed_attempts: u32,
        lockout_seconds: i64,
    ) -> PyResult<Self> {
        if !(60..=86_400).contains(&token_ttl_seconds) {
            return Err(InvalidInputError::new_err(
                "token_ttl_seconds debe estar entre 60 y 86400",
            ));
        }
        let cfg = AuthConfig {
            issuer: issuer.into(),
            audience: audience.into(),
            token_ttl_seconds,
            max_failed_attempts: max_failed_attempts.max(1),
            lockout_seconds: lockout_seconds.max(0),
        };
        let a =
            Authenticator::new(open_store(db_path)?, jwt_secret.as_bytes(), cfg).map_err(to_py)?;
        Ok(Self { inner: Arc::new(a) })
    }

    #[getter]
    fn token_ttl_seconds(&self) -> u64 {
        self.inner.config().token_ttl_seconds
    }

    /// Registra un usuario. Devuelve el nombre normalizado. Nunca sobrescribe uno existente.
    fn register_user(&self, py: Python<'_>, username: &str, password: &str) -> PyResult<String> {
        let a = self.inner.clone();
        let (u, p) = (username.to_owned(), password.to_owned());
        py.detach(move || a.register(&u, &p)).map_err(to_py)
    }

    /// Verifica credenciales y devuelve un token JWT.
    fn authenticate_user(
        &self,
        py: Python<'_>,
        username: &str,
        password: &str,
    ) -> PyResult<String> {
        let a = self.inner.clone();
        let (u, p) = (username.to_owned(), password.to_owned());
        py.detach(move || a.authenticate(&u, &p)).map_err(to_py)
    }

    /// Valida el token y devuelve el usuario. Lanza InvalidTokenError si no es válido.
    fn decode_token(&self, token: &str) -> PyResult<String> {
        self.inner.decode_token(token).map_err(to_py)
    }

    /// Compatibilidad con la API anterior: True si el token es válido.
    fn verify_token(&self, token: &str) -> bool {
        self.inner.decode_token(token).is_ok()
    }

    fn change_password(
        &self,
        py: Python<'_>,
        username: &str,
        old_password: &str,
        new_password: &str,
    ) -> PyResult<()> {
        let a = self.inner.clone();
        let (u, o, n) = (
            username.to_owned(),
            old_password.to_owned(),
            new_password.to_owned(),
        );
        py.detach(move || a.change_password(&u, &o, &n))
            .map_err(to_py)
    }

    fn user_exists(&self, username: &str) -> PyResult<bool> {
        self.inner.user_exists(username).map_err(to_py)
    }

    fn delete_user(&self, username: &str) -> PyResult<()> {
        self.inner.delete_user(username).map_err(to_py)
    }

    /// Invalida todos los tokens emitidos para el usuario.
    fn revoke_tokens(&self, username: &str) -> PyResult<()> {
        self.inner.revoke_tokens(username).map_err(to_py)
    }
}

#[pyclass(frozen)]
pub struct PyAudit {
    store: Arc<db::Store>,
}

#[pymethods]
impl PyAudit {
    #[new]
    fn new(db_path: &str) -> PyResult<Self> {
        Ok(Self {
            store: open_store(db_path)?,
        })
    }

    /// Últimos eventos de auditoría, del más reciente al más antiguo.
    #[pyo3(signature = (limit = 100))]
    fn events<'py>(&self, py: Python<'py>, limit: u32) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let rows = self
            .store
            .audit_events(limit.min(1000))
            .map_err(|e| AuthError_::new_err(e.to_string()))?;
        rows.into_iter()
            .map(|r| {
                let d = PyDict::new(py);
                d.set_item("ts", r.ts)?;
                d.set_item("username", r.username)?;
                d.set_item("action", r.action)?;
                d.set_item("success", r.success)?;
                d.set_item("detail", r.detail)?;
                Ok(d)
            })
            .collect()
    }
}

#[pyclass(frozen)]
pub struct PyIPA {
    inner: ipa::Policy,
}

#[pymethods]
impl PyIPA {
    #[new]
    fn new(db_path: &str) -> PyResult<Self> {
        Ok(Self {
            inner: ipa::Policy::new(open_store(db_path)?),
        })
    }

    fn add_role(&self, username: &str, role: &str) -> PyResult<()> {
        self.inner.add_role(username, role).map_err(to_py)
    }

    fn remove_role(&self, username: &str, role: &str) -> PyResult<()> {
        self.inner.remove_role(username, role).map_err(to_py)
    }

    fn get_roles(&self, username: &str) -> PyResult<Vec<String>> {
        self.inner.roles(username).map_err(to_py)
    }

    fn has_role(&self, username: &str, role: &str) -> PyResult<bool> {
        self.inner.has_role(username, role).map_err(to_py)
    }

    fn add_resource(&self, resource: &str, allowed_role: &str) -> PyResult<()> {
        self.inner
            .add_resource(resource, allowed_role)
            .map_err(to_py)
    }

    fn remove_resource(&self, resource: &str) -> PyResult<()> {
        self.inner.remove_resource(resource).map_err(to_py)
    }

    fn has_access(&self, username: &str, resource: &str) -> PyResult<bool> {
        self.inner.has_access(username, resource).map_err(to_py)
    }

    fn list_accessible_resources(&self, username: &str) -> PyResult<Vec<String>> {
        self.inner.accessible_resources(username).map_err(to_py)
    }
}

#[pymodule]
fn pam_ipa_project(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_class::<PyAuthManager>()?;
    m.add_class::<PyIPA>()?;
    m.add_class::<PyAudit>()?;
    m.add("AuthError", py.get_type::<AuthError_>())?;
    m.add("InvalidInputError", py.get_type::<InvalidInputError>())?;
    m.add("UserExistsError", py.get_type::<UserExistsError>())?;
    m.add(
        "InvalidCredentialsError",
        py.get_type::<InvalidCredentialsError>(),
    )?;
    m.add("InvalidTokenError", py.get_type::<InvalidTokenError>())?;
    m.add("NotFoundError", py.get_type::<NotFoundError>())?;
    Ok(())
}
