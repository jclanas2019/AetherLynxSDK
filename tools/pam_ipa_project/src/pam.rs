use crate::db::DBManager;
use argon2::{self, Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use password_hash::{SaltString, rand_core::OsRng};

pub struct PAMAuthManager {
    db_manager: DBManager,
}

impl PAMAuthManager {
    pub fn new() -> Result<PAMAuthManager, String> {
        let db_manager = DBManager::new().map_err(|e| e.to_string())?;
        Ok(PAMAuthManager { db_manager })
    }

    pub fn hash_password(&self, password: &str) -> String {
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let password_hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string();
        password_hash
    }

    pub fn verify_password(&self, password: &str, hash: &str) -> bool {
        let parsed_hash = PasswordHash::new(hash).unwrap();
        Argon2::default().verify_password(password.as_bytes(), &parsed_hash).is_ok()
    }

    pub fn get_user(&self, username: &str) -> Result<Option<(String, String)>, String> {
        self.db_manager.get_user(username).map_err(|e| e.to_string())
    }

    pub fn store_user(&self, username: &str, password: &str) -> Result<(), String> {
        let hashed_password = self.hash_password(password);
        self.db_manager.store_user(username, &hashed_password)
            .map_err(|e| e.to_string())
    }
}