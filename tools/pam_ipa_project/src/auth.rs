use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use std::collections::HashMap;

pub struct AuthManager {
    users: HashMap<String, String>,
}

impl AuthManager {
    pub fn new() -> Self {
        AuthManager {
            users: HashMap::new(),
        }
    }

    pub fn register_user(&mut self, username: &str, password: &str) -> Result<(), String> {
        if self.users.contains_key(username) {
            return Err("User already exists".to_string());
        }

        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let password_hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| e.to_string())?
            .to_string();

        self.users.insert(username.to_string(), password_hash);
        Ok(())
    }

    pub fn authenticate_user(&self, username: &str, password: &str) -> Result<bool, String> {
        if let Some(stored_hash) = self.users.get(username) {
            self.verify_password(stored_hash, password)
                .map_err(|_| "Password verification failed".to_string())
        } else {
            Err("User not found".to_string())
        }
    }

    fn verify_password(&self, hash: &str, password: &str) -> Result<bool, argon2::password_hash::Error> {
        let parsed_hash = PasswordHash::new(hash)?;
        Ok(Argon2::default().verify_password(password.as_bytes(), &parsed_hash).is_ok())
    }
}