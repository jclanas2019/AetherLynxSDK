//! Autorización por roles (RBAC) persistida en SQLite.

use std::sync::Arc;

use crate::db::Store;
use crate::pam::AuthError;
use crate::validate;

pub struct Policy {
    store: Arc<Store>,
}

impl Policy {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn user(&self, raw: &str) -> Result<String, AuthError> {
        validate::username(raw).map_err(AuthError::Invalid)
    }

    pub fn add_role(&self, username: &str, role: &str) -> Result<(), AuthError> {
        let u = self.user(username)?;
        let r = validate::label("rol", role).map_err(AuthError::Invalid)?;
        if !self.store.user_exists(&u)? {
            return Err(AuthError::NotFound);
        }
        self.store.add_role(&u, &r)?;
        Ok(())
    }

    pub fn remove_role(&self, username: &str, role: &str) -> Result<(), AuthError> {
        let u = self.user(username)?;
        let r = validate::label("rol", role).map_err(AuthError::Invalid)?;
        self.store.remove_role(&u, &r)?;
        Ok(())
    }

    pub fn roles(&self, username: &str) -> Result<Vec<String>, AuthError> {
        Ok(self.store.roles(&self.user(username)?)?)
    }

    pub fn has_role(&self, username: &str, role: &str) -> Result<bool, AuthError> {
        let r = validate::label("rol", role).map_err(AuthError::Invalid)?;
        Ok(self.roles(username)?.contains(&r))
    }

    pub fn add_resource(&self, resource: &str, role: &str) -> Result<(), AuthError> {
        let res = validate::label("recurso", resource).map_err(AuthError::Invalid)?;
        let r = validate::label("rol", role).map_err(AuthError::Invalid)?;
        self.store.add_resource(&res, &r)?;
        Ok(())
    }

    pub fn remove_resource(&self, resource: &str) -> Result<(), AuthError> {
        let res = validate::label("recurso", resource).map_err(AuthError::Invalid)?;
        self.store.remove_resource(&res)?;
        Ok(())
    }

    /// Denegar por defecto: entradas inválidas o desconocidas no dan acceso.
    pub fn has_access(&self, username: &str, resource: &str) -> Result<bool, AuthError> {
        let (Ok(u), Ok(res)) = (
            validate::username(username),
            validate::label("recurso", resource),
        ) else {
            return Ok(false);
        };
        Ok(self.store.has_access(&u, &res)?)
    }

    pub fn accessible_resources(&self, username: &str) -> Result<Vec<String>, AuthError> {
        Ok(self.store.accessible_resources(&self.user(username)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(path: &str) -> Policy {
        let store = Arc::new(Store::open(path).unwrap());
        store.insert_user("alice", "x", 0, "id-alice").unwrap();
        Policy::new(store)
    }

    #[test]
    fn access_through_roles() {
        let p = setup(":memory:");
        p.add_role("alice", "student").unwrap();
        p.add_resource("grades", "student").unwrap();
        p.add_resource("payroll", "finance").unwrap();
        assert!(p.has_access("alice", "grades").unwrap());
        assert!(!p.has_access("alice", "payroll").unwrap());
        assert_eq!(p.accessible_resources("alice").unwrap(), vec!["grades"]);
        p.remove_role("alice", "student").unwrap();
        assert!(!p.has_access("alice", "grades").unwrap());
    }

    #[test]
    fn roles_require_existing_user() {
        let p = setup(":memory:");
        assert_eq!(p.add_role("ghost", "admin"), Err(AuthError::NotFound));
    }

    #[test]
    fn policy_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.db");
        let path = path.to_str().unwrap();
        {
            let p = setup(path);
            p.add_role("alice", "student").unwrap();
            p.add_resource("grades", "student").unwrap();
        }
        let p = Policy::new(Arc::new(Store::open(path).unwrap()));
        assert!(p.has_access("alice", "grades").unwrap());
    }

    #[test]
    fn invalid_input_denies() {
        let p = setup(":memory:");
        assert!(!p.has_access("alice", "bad resource!").unwrap());
        assert!(!p.has_access("x", "grades").unwrap());
    }
}
