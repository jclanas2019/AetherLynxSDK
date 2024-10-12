use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;
use ring::{aead, rand::{SecureRandom, SystemRandom}};
use rcgen::{Certificate, CertificateParams, PKCS_ECDSA_P256_SHA256};
use base64::{encode, decode};
use chrono::{Utc, Datelike};
use rusqlite::{Connection, params};

#[pyclass]
struct RustVault {
    key: aead::LessSafeKey,
    conn: Connection,
}

#[pymethods]
impl RustVault {
    #[new]
    fn new(db_path: &str) -> PyResult<Self> {
        let rng = SystemRandom::new();
        let mut key_bytes = vec![0; aead::AES_256_GCM.key_len()];
        rng.fill(&mut key_bytes).map_err(|_| PyValueError::new_err("Failed to generate key"))?;
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, &key_bytes)
            .map_err(|_| PyValueError::new_err("Failed to create UnboundKey"))?;
        
        let conn = Connection::open(db_path)
            .map_err(|_| PyValueError::new_err("Failed to open SQLite database"))?;
        
        conn.execute(
            "CREATE TABLE IF NOT EXISTS secrets (
                name TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                created_at TEXT NOT NULL
            )",
            [],
        ).map_err(|_| PyValueError::new_err("Failed to create secrets table"))?;

        Ok(RustVault {
            key: aead::LessSafeKey::new(key),
            conn,
        })
    }

    fn encrypt(&self, data: &[u8]) -> PyResult<String> {
        let mut nonce_bytes = [0u8; 12];
        SystemRandom::new().fill(&mut nonce_bytes).map_err(|_| PyValueError::new_err("Failed to generate nonce"))?;
        let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
        let aad = aead::Aad::empty();
        
        let mut in_out = data.to_vec();
        self.key.seal_in_place_append_tag(nonce, aad, &mut in_out)
            .map_err(|_| PyValueError::new_err("Encryption failed"))?;
        
        let mut result = Vec::with_capacity(12 + in_out.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&in_out);
        
        Ok(encode(result))
    }

    fn decrypt(&self, data: &str) -> PyResult<Vec<u8>> {
        let decoded = decode(data)
            .map_err(|_| PyValueError::new_err("Invalid base64 input"))?;
        
        if decoded.len() < 12 {
            return Err(PyValueError::new_err("Invalid encrypted data"));
        }

        let nonce = aead::Nonce::try_assume_unique_for_key(&decoded[..12])
            .map_err(|_| PyValueError::new_err("Invalid nonce"))?;
        let aad = aead::Aad::empty();
        
        let mut in_out = decoded[12..].to_vec();
        let decrypted_data = self.key.open_in_place(nonce, aad, &mut in_out)
            .map_err(|_| PyValueError::new_err("Decryption failed"))?;
        
        Ok(decrypted_data.to_vec())
    }

    fn generate_certificate(&self, common_name: &str) -> PyResult<String> {
        let mut params = CertificateParams::new(vec![common_name.to_string()]);
        let now = Utc::now();
        params.not_before = rcgen::date_time_ymd(now.year(), now.month() as u8, now.day() as u8);
        params.not_after = rcgen::date_time_ymd(now.year() + 1, now.month() as u8, now.day() as u8);
        params.alg = &PKCS_ECDSA_P256_SHA256;
        
        let cert = Certificate::from_params(params)
            .map_err(|_| PyValueError::new_err("Failed to generate certificate"))?;
        
        let pem = cert.serialize_pem()
            .map_err(|_| PyValueError::new_err("Failed to serialize certificate"))?;
        
        Ok(pem)
    }

    fn add_secret(&mut self, name: String, value: String) -> PyResult<()> {
        let encrypted_value = self.encrypt(value.as_bytes())?;
        let created_at = Utc::now().to_rfc3339();
        
        self.conn.execute(
            "INSERT OR REPLACE INTO secrets (name, value, created_at) VALUES (?1, ?2, ?3)",
            params![name, encrypted_value, created_at],
        ).map_err(|_| PyValueError::new_err("Failed to add secret"))?;

        Ok(())
    }

    fn get_secret(&self, name: &str) -> PyResult<String> {
        let mut stmt = self.conn.prepare("SELECT value FROM secrets WHERE name = ?1")
            .map_err(|_| PyValueError::new_err("Failed to prepare statement"))?;
        
        let encrypted_value: String = stmt.query_row(params![name], |row| row.get(0))
            .map_err(|_| PyValueError::new_err("Secret not found"))?;
        
        let decrypted = self.decrypt(&encrypted_value)?;
        String::from_utf8(decrypted)
            .map_err(|_| PyValueError::new_err("Failed to decode secret"))
    }

    fn list_secrets(&self) -> PyResult<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT name FROM secrets")
            .map_err(|_| PyValueError::new_err("Failed to prepare statement"))?;
        
        let secret_iter = stmt.query_map([], |row| row.get(0))
            .map_err(|_| PyValueError::new_err("Failed to query secrets"))?;
        
        let secrets: Result<Vec<String>, _> = secret_iter.collect();
        secrets.map_err(|_| PyValueError::new_err("Failed to collect secrets"))
    }

    fn delete_secret(&self, name: &str) -> PyResult<()> {
        self.conn.execute("DELETE FROM secrets WHERE name = ?1", params![name])
            .map_err(|_| PyValueError::new_err("Failed to delete secret"))?;
        Ok(())
    }
}

#[pymodule]
fn rust_vault(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<RustVault>()?;
    Ok(())
}