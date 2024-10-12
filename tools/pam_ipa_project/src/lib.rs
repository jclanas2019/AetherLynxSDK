use pyo3::prelude::*;
mod pam;
mod db;
mod audit;
mod ipa;
use jsonwebtoken::{encode, decode, Header, Validation, EncodingKey, DecodingKey};
use serde::{Serialize, Deserialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
}

#[pyclass]
pub struct PyAuthManager {
    pam_auth_manager: pam::PAMAuthManager,
    audit_log: audit::AuditLog,
}

#[pymethods]
impl PyAuthManager {
    #[new]
    pub fn new() -> PyResult<Self> {
        let pam_auth_manager = pam::PAMAuthManager::new()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e))?;
        Ok(PyAuthManager {
            pam_auth_manager,
            audit_log: audit::AuditLog::new(),
        })
    }

    pub fn register_user(&mut self, username: &str, password: &str) -> PyResult<()> {
        self.pam_auth_manager.store_user(username, password)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(e))
    }

    pub fn authenticate_user(&mut self, username: &str, password: &str) -> PyResult<String> {
        match self.pam_auth_manager.get_user(username) {
            Ok(Some((_, stored_password))) => {
                if self.pam_auth_manager.verify_password(password, &stored_password) {
                    let token = self.generate_token(username)?;
                    self.audit_log.log_event(username, "login", true);
                    println!("Autenticación exitosa para el usuario: {}", username);
                    Ok(token)
                } else {
                    self.audit_log.log_event(username, "login", false);
                    println!("Error en la autenticación de contraseña para el usuario: {}", username);
                    Err(PyErr::new::<pyo3::exceptions::PyValueError, _>("Autenticación fallida"))
                }
            },
            Ok(None) => {
                println!("Usuario no encontrado: {}", username);
                Err(PyErr::new::<pyo3::exceptions::PyValueError, _>("Usuario no encontrado"))
            },
            Err(e) => Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e)),
        }
    }

    pub fn verify_token(&self, token: &str) -> bool {
        let key = DecodingKey::from_secret(b"secret");
        decode::<Claims>(token, &key, &Validation::default()).is_ok()
    }

    fn generate_token(&self, username: &str) -> PyResult<String> {
        let expiration = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 600; // Expira en 10 minutos
        let claims = Claims {
            sub: username.to_owned(),
            exp: expiration as usize,
        };
        let key = EncodingKey::from_secret(b"secret");
        encode(&Header::default(), &claims, &key)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(format!("Error generando token: {}", e)))
    }

    pub fn show_logs(&self) {
        self.audit_log.show_logs();
    }
}

#[pyclass]
pub struct PyIPA {
    policy: ipa::IPAPolicy,
}

#[pymethods]
impl PyIPA {
    #[new]
    pub fn new() -> Self {
        PyIPA {
            policy: ipa::IPAPolicy::new(),
        }
    }

    pub fn add_role(&mut self, username: &str, role: &str) {
        self.policy.add_role(username, role);
    }

    pub fn remove_role(&mut self, username: &str, role: &str) {
        self.policy.remove_role(username, role);
    }

    pub fn get_roles(&self, username: &str) -> Option<Vec<String>> {
        self.policy.get_roles(username).cloned()
    }

    pub fn add_resource(&mut self, resource: &str, allowed_role: &str) {
        self.policy.add_resource(resource, allowed_role);
    }

    pub fn remove_resource(&mut self, resource: &str) {
        self.policy.remove_resource(resource);
    }

    pub fn has_access(&self, username: &str, resource: &str) -> bool {
        self.policy.has_access(username, resource)
    }

    pub fn list_accessible_resources(&self, username: &str) -> Vec<String> {
        self.policy.list_accessible_resources(username)
    }
}

#[pymodule]
fn pam_ipa_project(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<PyAuthManager>()?;
    m.add_class::<PyIPA>()?;
    Ok(())
}