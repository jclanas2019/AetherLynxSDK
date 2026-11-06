//! Autenticación: contraseñas con Argon2id, bloqueo tras intentos fallidos y
//! tokens JWT HS256 con emisor, audiencia, expiración e identificador únicos.

use std::sync::Arc;

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use base64::Engine;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::db::Store;
use crate::validate;

pub const MIN_SECRET_LEN: usize = 32;

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Invalid(String),
    UserExists,
    /// Mensaje deliberadamente genérico: no distingue usuario inexistente,
    /// contraseña errónea ni cuenta bloqueada.
    InvalidCredentials,
    InvalidToken,
    NotFound,
    Internal(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => f.write_str(m),
            Self::UserExists => f.write_str("el usuario ya existe"),
            Self::InvalidCredentials => f.write_str("credenciales inválidas"),
            Self::InvalidToken => f.write_str("token inválido o expirado"),
            Self::NotFound => f.write_str("usuario no encontrado"),
            Self::Internal(m) => write!(f, "error interno: {m}"),
        }
    }
}

impl From<rusqlite::Error> for AuthError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(format!("base de datos: {e}"))
    }
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub issuer: String,
    pub audience: String,
    pub token_ttl_seconds: u64,
    pub max_failed_attempts: u32,
    pub lockout_seconds: i64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            issuer: "aetherlynx".into(),
            audience: "aetherlynx-api".into(),
            token_ttl_seconds: 600,
            max_failed_attempts: 5,
            lockout_seconds: 900,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub iss: String,
    pub aud: String,
    pub iat: u64,
    pub nbf: u64,
    pub exp: u64,
    pub jti: String,
    /// Identificador de la cuenta: un token no sirve para otra cuenta creada
    /// después con el mismo nombre.
    pub uid: String,
    /// Versión de credenciales: cambiar la contraseña o revocar sesiones la
    /// incrementa e invalida los tokens anteriores.
    pub ver: i64,
}

pub struct Authenticator {
    store: Arc<Store>,
    enc: EncodingKey,
    dec: DecodingKey,
    validation: Validation,
    cfg: AuthConfig,
    /// Hash de referencia para verificar contra él cuando el usuario no existe,
    /// de modo que el tiempo de respuesta no revele qué cuentas existen.
    dummy_hash: String,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Internal(format!("hash de contraseña: {e}")))
}

/// Un hash corrupto en la base de datos cuenta como "no coincide", nunca como pánico.
fn verify_password(hash: &str, password: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

impl Authenticator {
    pub fn new(store: Arc<Store>, secret: &[u8], cfg: AuthConfig) -> Result<Self, AuthError> {
        if secret.len() < MIN_SECRET_LEN {
            return Err(AuthError::Invalid(format!(
                "el secreto JWT debe tener al menos {MIN_SECRET_LEN} bytes"
            )));
        }
        if secret.iter().all(|b| *b == secret[0]) {
            return Err(AuthError::Invalid(
                "el secreto JWT no puede ser un byte repetido".into(),
            ));
        }
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[&cfg.issuer]);
        validation.set_audience(&[&cfg.audience]);
        validation.set_required_spec_claims(&["exp", "nbf", "sub", "iss", "aud"]);
        validation.validate_nbf = true;
        validation.leeway = 30;
        let mut rnd = [0u8; 32];
        OsRng.fill_bytes(&mut rnd);
        let dummy_hash = hash_password(&base64::engine::general_purpose::STANDARD.encode(rnd))?;
        Ok(Self {
            store,
            enc: EncodingKey::from_secret(secret),
            dec: DecodingKey::from_secret(secret),
            validation,
            cfg,
            dummy_hash,
        })
    }

    pub fn config(&self) -> &AuthConfig {
        &self.cfg
    }

    fn audit(&self, user: &str, action: &str, ok: bool, detail: &str) {
        // Un fallo al auditar no debe tumbar el login, pero tampoco ocultarse.
        if let Err(e) =
            self.store
                .audit(now() as i64, &validate::for_audit(user), action, ok, detail)
        {
            eprintln!("pam_ipa_project: no se pudo escribir auditoría: {e}");
        }
    }

    pub fn register(&self, username: &str, password: &str) -> Result<String, AuthError> {
        let user = validate::username(username).map_err(AuthError::Invalid)?;
        validate::password(password).map_err(AuthError::Invalid)?;
        let hash = hash_password(password)?;
        let mut id = [0u8; 16];
        OsRng.fill_bytes(&mut id);
        let account_id: String = id.iter().map(|b| format!("{b:02x}")).collect();
        if !self
            .store
            .insert_user(&user, &hash, now() as i64, &account_id)?
        {
            self.audit(&user, "register", false, "ya existe");
            return Err(AuthError::UserExists);
        }
        self.audit(&user, "register", true, "");
        Ok(user)
    }

    /// Verifica credenciales y devuelve un token de acceso.
    pub fn authenticate(&self, username: &str, password: &str) -> Result<String, AuthError> {
        let ts = now() as i64;
        let user = match validate::username(username) {
            Ok(u) => u,
            Err(_) => {
                verify_password(&self.dummy_hash, password);
                self.audit(username, "login", false, "usuario malformado");
                return Err(AuthError::InvalidCredentials);
            }
        };
        if password.chars().count() > validate::PASSWORD_MAX {
            self.audit(&user, "login", false, "contraseña demasiado larga");
            return Err(AuthError::InvalidCredentials);
        }
        // El intento se cuenta ANTES de verificar (ver Store::reserve_attempt).
        let lock_until = ts + self.cfg.lockout_seconds;
        let attempt =
            self.store
                .reserve_attempt(&user, ts, self.cfg.max_failed_attempts, lock_until)?;
        let row = match (attempt, self.store.get_user(&user)?) {
            (Some(_), Some(row)) => row,
            (None, Some(_)) => {
                verify_password(&self.dummy_hash, password);
                self.audit(&user, "login", false, "cuenta bloqueada");
                return Err(AuthError::InvalidCredentials);
            }
            (_, None) => {
                verify_password(&self.dummy_hash, password);
                self.audit(&user, "login", false, "usuario inexistente");
                return Err(AuthError::InvalidCredentials);
            }
        };
        if !verify_password(&row.password_hash, password) {
            let detail = if attempt.unwrap_or(0) >= self.cfg.max_failed_attempts {
                "contraseña incorrecta; cuenta bloqueada"
            } else {
                "contraseña incorrecta"
            };
            self.audit(&user, "login", false, detail);
            return Err(AuthError::InvalidCredentials);
        }
        self.store.reset_failures(&user)?;
        let token = self.issue_token(&row.username, &row.account_id, row.token_version)?;
        self.audit(&user, "login", true, "");
        Ok(token)
    }

    pub fn issue_token(
        &self,
        username: &str,
        account_id: &str,
        version: i64,
    ) -> Result<String, AuthError> {
        let iat = now();
        let mut jti = [0u8; 16];
        OsRng.fill_bytes(&mut jti);
        let claims = Claims {
            sub: username.to_owned(),
            iss: self.cfg.issuer.clone(),
            aud: self.cfg.audience.clone(),
            iat,
            nbf: iat,
            exp: iat + self.cfg.token_ttl_seconds,
            jti: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(jti),
            uid: account_id.to_owned(),
            ver: version,
        };
        encode(&Header::new(Algorithm::HS256), &claims, &self.enc)
            .map_err(|e| AuthError::Internal(format!("firma de token: {e}")))
    }

    /// Valida firma, algoritmo, emisor, audiencia y vigencia, que la cuenta
    /// siga existiendo (la misma, no otra con su nombre) y que no se hayan
    /// revocado sus sesiones. Devuelve el nombre de usuario.
    pub fn decode_token(&self, token: &str) -> Result<String, AuthError> {
        if token.len() > 4096 {
            return Err(AuthError::InvalidToken);
        }
        let data = decode::<Claims>(token, &self.dec, &self.validation)
            .map_err(|_| AuthError::InvalidToken)?;
        let c = data.claims;
        match self.store.get_user(&c.sub)? {
            Some(row) if row.account_id == c.uid && row.token_version == c.ver => Ok(c.sub),
            _ => Err(AuthError::InvalidToken),
        }
    }

    pub fn change_password(&self, username: &str, old: &str, new: &str) -> Result<(), AuthError> {
        let user = validate::username(username).map_err(|_| AuthError::InvalidCredentials)?;
        validate::password(new).map_err(AuthError::Invalid)?;
        let row = self
            .store
            .get_user(&user)?
            .ok_or(AuthError::InvalidCredentials)?;
        if !verify_password(&row.password_hash, old) {
            self.audit(
                &user,
                "change_password",
                false,
                "contraseña actual incorrecta",
            );
            return Err(AuthError::InvalidCredentials);
        }
        self.store.update_password(&user, &hash_password(new)?)?;
        self.audit(&user, "change_password", true, "");
        Ok(())
    }

    /// Invalida todos los tokens de la cuenta ("cerrar todas las sesiones").
    pub fn revoke_tokens(&self, username: &str) -> Result<(), AuthError> {
        let user = validate::username(username).map_err(AuthError::Invalid)?;
        if !self.store.bump_token_version(&user)? {
            return Err(AuthError::NotFound);
        }
        self.audit(&user, "revoke_tokens", true, "");
        Ok(())
    }

    pub fn user_exists(&self, username: &str) -> Result<bool, AuthError> {
        match validate::username(username) {
            Ok(u) => Ok(self.store.user_exists(&u)?),
            Err(_) => Ok(false),
        }
    }

    pub fn delete_user(&self, username: &str) -> Result<(), AuthError> {
        let user = validate::username(username).map_err(AuthError::Invalid)?;
        if !self.store.delete_user(&user)? {
            return Err(AuthError::NotFound);
        }
        self.audit(&user, "delete_user", true, "");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"unit-test-secret-0123456789abcdef!!";
    const PW: &str = "correct horse battery";

    fn setup() -> (tempfile::TempDir, Authenticator) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap());
        let cfg = AuthConfig {
            max_failed_attempts: 3,
            ..Default::default()
        };
        (dir, Authenticator::new(store, SECRET, cfg).unwrap())
    }

    #[test]
    fn weak_secrets_are_rejected() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        assert!(Authenticator::new(store.clone(), b"secret", AuthConfig::default()).is_err());
        assert!(Authenticator::new(store, &[b'a'; 64], AuthConfig::default()).is_err());
    }

    #[test]
    fn register_login_and_decode() {
        let (_d, a) = setup();
        assert_eq!(a.register("Alice", PW).unwrap(), "alice");
        let t = a.authenticate("alice", PW).unwrap();
        assert_eq!(a.decode_token(&t).unwrap(), "alice");
    }

    #[test]
    fn re_register_does_not_overwrite_password() {
        let (_d, a) = setup();
        a.register("alice", PW).unwrap();
        assert_eq!(
            a.register("alice", "another password!!"),
            Err(AuthError::UserExists)
        );
        assert!(a.authenticate("alice", PW).is_ok());
        assert!(a.authenticate("alice", "another password!!").is_err());
    }

    #[test]
    fn errors_do_not_reveal_whether_user_exists() {
        let (_d, a) = setup();
        a.register("alice", PW).unwrap();
        assert_eq!(
            a.authenticate("nobody", PW),
            Err(AuthError::InvalidCredentials)
        );
        assert_eq!(
            a.authenticate("alice", "wrong password!!"),
            Err(AuthError::InvalidCredentials)
        );
    }

    #[test]
    fn lockout_after_max_attempts() {
        let (_d, a) = setup();
        a.register("alice", PW).unwrap();
        for _ in 0..3 {
            let _ = a.authenticate("alice", "wrong password!!");
        }
        // Bloqueada: incluso la contraseña correcta falla.
        assert_eq!(
            a.authenticate("alice", PW),
            Err(AuthError::InvalidCredentials)
        );
    }

    #[test]
    fn token_signed_with_other_key_is_rejected() {
        let (_d, a) = setup();
        a.register("alice", PW).unwrap();
        let store = Arc::new(Store::open(":memory:").unwrap());
        let other = Authenticator::new(
            store,
            b"a-completely-different-key-0987654321",
            AuthConfig::default(),
        )
        .unwrap();
        let foreign = other.issue_token("alice", "x", 0).unwrap();
        assert_eq!(a.decode_token(&foreign), Err(AuthError::InvalidToken));
    }

    #[test]
    fn token_for_deleted_user_is_rejected() {
        let (_d, a) = setup();
        a.register("alice", PW).unwrap();
        let t = a.authenticate("alice", PW).unwrap();
        a.delete_user("alice").unwrap();
        assert_eq!(a.decode_token(&t), Err(AuthError::InvalidToken));
    }

    #[test]
    fn migrates_v01_database_and_keeps_logins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let path = path.to_str().unwrap();
        {
            // Esquema exacto de la versión 0.1, con un hash Argon2 como los que generaba.
            let c = rusqlite::Connection::open(path).unwrap();
            c.execute_batch(
                "CREATE TABLE users (username TEXT PRIMARY KEY, password TEXT NOT NULL)",
            )
            .unwrap();
            c.execute(
                "INSERT INTO users VALUES ('Alice', ?1)",
                [hash_password(PW).unwrap()],
            )
            .unwrap();
        }
        let a = Authenticator::new(
            Arc::new(Store::open(path).unwrap()),
            SECRET,
            AuthConfig::default(),
        )
        .unwrap();
        assert!(a.authenticate("alice", PW).is_ok());
        // Reabrir una base ya migrada no hace nada.
        Store::open(path).unwrap();
    }

    #[test]
    fn lock_expiry_resets_the_counter() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap());
        let cfg = AuthConfig {
            max_failed_attempts: 3,
            lockout_seconds: 0,
            ..Default::default()
        };
        let a = Authenticator::new(store.clone(), SECRET, cfg).unwrap();
        a.register("alice", PW).unwrap();
        for _ in 0..3 {
            let _ = a.authenticate("alice", "wrong password!!");
        }
        // lockout_seconds = 0: el bloqueo ya venció. Un fallo más NO debe
        // volver a bloquear de inmediato; la contraseña correcta funciona.
        let _ = a.authenticate("alice", "wrong password!!");
        assert_eq!(store.get_user("alice").unwrap().unwrap().failed_attempts, 1);
        assert!(a.authenticate("alice", PW).is_ok());
    }

    #[test]
    fn parallel_attempts_cannot_exceed_the_limit() {
        let (_d, a) = setup(); // max 3
        a.register("alice", PW).unwrap();
        let a = Arc::new(a);
        // Contrapartida del conteo pesimista: logins correctos simultáneos
        // pueden rechazarse momentáneamente, pero el primero que acierta
        // desbloquea y la cuenta no queda bloqueada.
        let handles: Vec<_> = (0..12)
            .map(|_| {
                let a = a.clone();
                std::thread::spawn(move || a.authenticate("alice", PW).is_ok())
            })
            .collect();
        let ok = handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .filter(|x| *x)
            .count();
        assert!(ok >= 1);
        assert!(a.authenticate("alice", PW).is_ok(), "no queda bloqueada");
        // Con una contraseña errónea en paralelo, nunca se superan los 3
        // intentos antes del bloqueo, y después ni la correcta entra.
        let handles: Vec<_> = (0..12)
            .map(|_| {
                let a = a.clone();
                std::thread::spawn(move || {
                    let _ = a.authenticate("alice", "wrong password!!");
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(a.store.get_user("alice").unwrap().unwrap().failed_attempts <= 3);
        assert_eq!(
            a.authenticate("alice", PW),
            Err(AuthError::InvalidCredentials)
        );
    }

    #[test]
    fn password_change_and_revocation_invalidate_old_tokens() {
        let (_d, a) = setup();
        a.register("alice", PW).unwrap();
        let t1 = a.authenticate("alice", PW).unwrap();
        a.change_password("alice", PW, "new password 123").unwrap();
        assert_eq!(a.decode_token(&t1), Err(AuthError::InvalidToken));
        let t2 = a.authenticate("alice", "new password 123").unwrap();
        a.revoke_tokens("alice").unwrap();
        assert_eq!(a.decode_token(&t2), Err(AuthError::InvalidToken));
    }

    #[test]
    fn token_does_not_carry_over_to_a_recreated_account() {
        let (_d, a) = setup();
        a.register("alice", PW).unwrap();
        let t = a.authenticate("alice", PW).unwrap();
        a.delete_user("alice").unwrap();
        a.register("alice", PW).unwrap();
        assert_eq!(a.decode_token(&t), Err(AuthError::InvalidToken));
    }

    #[test]
    fn migration_skips_case_collisions_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let path = path.to_str().unwrap();
        {
            let c = rusqlite::Connection::open(path).unwrap();
            c.execute_batch(
                "CREATE TABLE users (username TEXT PRIMARY KEY, password TEXT NOT NULL)",
            )
            .unwrap();
            for u in ["Bob", "BOB", "Carol"] {
                c.execute(
                    "INSERT INTO users VALUES (?1, ?2)",
                    [u, &hash_password(PW).unwrap()],
                )
                .unwrap();
            }
        }
        let store = Store::open(path).unwrap();
        assert!(store.get_user("carol").unwrap().is_some());
        assert!(
            store.get_user("Bob").unwrap().is_some(),
            "las colisiones quedan intactas"
        );
        assert!(store.get_user("BOB").unwrap().is_some());
    }

    #[test]
    fn corrupt_hash_is_a_failed_login_not_a_panic() {
        assert!(!verify_password("not-a-phc-string", PW));
    }

    #[test]
    fn change_password_requires_old_one() {
        let (_d, a) = setup();
        a.register("alice", PW).unwrap();
        assert!(a
            .change_password("alice", "wrong password!!", "new password 123")
            .is_err());
        a.change_password("alice", PW, "new password 123").unwrap();
        assert!(a.authenticate("alice", "new password 123").is_ok());
    }
}
