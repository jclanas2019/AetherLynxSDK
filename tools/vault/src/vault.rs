//! Núcleo del vault.
//!
//! - La clave AES-256-GCM se deriva de una passphrase con Argon2id. La sal y
//!   los parámetros se guardan en la tabla `vault_meta`, así que la misma
//!   passphrase reabre el vault tras un reinicio.
//! - Un "canario" cifrado permite detectar una passphrase incorrecta sin
//!   tocar los datos.
//! - Cada secreto se cifra con un nonce aleatorio de 96 bits y su nombre
//!   como datos autenticados (AAD): mover un ciphertext a otra fila hace
//!   fallar el descifrado.

use std::sync::{Mutex, MutexGuard};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{params, Connection, OptionalExtension};
use zeroize::Zeroizing;

pub const PASSPHRASE_MIN: usize = 12;
pub const VALUE_MAX: usize = 64 * 1024;
const FORMAT_VERSION: i64 = 2;
const CANARY: &[u8] = b"aetherlynx-vault-canary";

#[derive(Debug, PartialEq, Eq)]
pub enum VaultError {
    Invalid(String),
    WrongPassphrase,
    NotFound,
    /// Vault de la versión 0.1: su clave era aleatoria y nunca se guardó.
    Legacy,
    /// Otro proceso cambió la passphrase después de abrir este handle.
    Stale,
    Crypto,
    Db(String),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => f.write_str(m),
            Self::WrongPassphrase => f.write_str("passphrase incorrecta"),
            Self::NotFound => f.write_str("secreto no encontrado"),
            Self::Legacy => f.write_str(
                "esta base de datos es de la versión 0.1, cuya clave era aleatoria y no se guardaba: \
                 sus secretos no se pueden descifrar. Use un archivo nuevo y vuelva a cargar los secretos",
            ),
            Self::Stale => f.write_str(
                "la passphrase se cambió desde otro proceso: vuelva a abrir el vault con la nueva",
            ),
            Self::Crypto => f.write_str("error criptográfico: datos corruptos o manipulados"),
            Self::Db(m) => write!(f, "base de datos: {m}"),
        }
    }
}

impl From<rusqlite::Error> for VaultError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for KdfParams {
    /// Recomendación de OWASP para Argon2id: 64 MiB, 3 pasadas, 1 hilo.
    fn default() -> Self {
        Self {
            m_cost_kib: 64 * 1024,
            t_cost: 3,
            p_cost: 1,
        }
    }
}

// Límites de Argon2id. El mínimo es el de OWASP (19 MiB, 2 pasadas); en los
// tests unitarios se relaja para que sean rápidos. El máximo evita que una
// base manipulada fuerce a reservar memoria sin límite.
const M_MIN_KIB: u32 = if cfg!(test) { 8 } else { 19 * 1024 };
const T_MIN: u32 = if cfg!(test) { 1 } else { 2 };
const M_MAX_KIB: u32 = 4 * 1024 * 1024;
const T_MAX: u32 = 10;
const P_MAX: u32 = 16;

impl KdfParams {
    pub fn check(&self) -> Result<(), String> {
        if !(M_MIN_KIB..=M_MAX_KIB).contains(&self.m_cost_kib) {
            return Err(format!(
                "m_cost_kib debe estar entre {M_MIN_KIB} y {M_MAX_KIB}"
            ));
        }
        if !(T_MIN..=T_MAX).contains(&self.t_cost) {
            return Err(format!("t_cost debe estar entre {T_MIN} y {T_MAX}"));
        }
        if !(1..=P_MAX).contains(&self.p_cost) {
            return Err(format!("p_cost debe estar entre 1 y {P_MAX}"));
        }
        Ok(())
    }
}

fn derive_key(passphrase: &str, salt: &[u8], p: KdfParams) -> Result<LessSafeKey, VaultError> {
    let params =
        Params::new(p.m_cost_kib, p.t_cost, p.p_cost, Some(32)).map_err(|_| VaultError::Crypto)?;
    let mut raw = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, raw.as_mut())
        .map_err(|_| VaultError::Crypto)?;
    let unbound = UnboundKey::new(&AES_256_GCM, raw.as_ref()).map_err(|_| VaultError::Crypto)?;
    Ok(LessSafeKey::new(unbound))
}

fn seal(key: &LessSafeKey, aad: &[u8], plaintext: &[u8]) -> Result<String, VaultError> {
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| VaultError::Crypto)?;
    let mut buf = plaintext.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad),
        &mut buf,
    )
    .map_err(|_| VaultError::Crypto)?;
    let mut out = Vec::with_capacity(NONCE_LEN + buf.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&buf);
    Ok(B64.encode(out))
}

fn open(key: &LessSafeKey, aad: &[u8], token: &str) -> Result<Vec<u8>, VaultError> {
    let data = B64.decode(token).map_err(|_| VaultError::Crypto)?;
    if data.len() < NONCE_LEN + aead::AES_256_GCM.tag_len() {
        return Err(VaultError::Crypto);
    }
    let (nonce, ct) = data.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| VaultError::Crypto)?;
    let mut buf = ct.to_vec();
    let plain = key
        .open_in_place(nonce, Aad::from(aad), &mut buf)
        .map_err(|_| VaultError::Crypto)?;
    Ok(plain.to_vec())
}

fn secret_aad(name: &str) -> Vec<u8> {
    format!("secret:{name}").into_bytes()
}

fn data_aad(context: &str) -> Vec<u8> {
    format!("data:{context}").into_bytes()
}

pub fn validate_name(name: &str) -> Result<(), VaultError> {
    let ok_len = (1..=128).contains(&name.len());
    let ok_chars = name
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'/' | b':'));
    if ok_len && ok_chars {
        Ok(())
    } else {
        Err(VaultError::Invalid(
            "nombre de secreto inválido: 1-128 caracteres de letras, dígitos y . _ - / :".into(),
        ))
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn random_salt() -> Result<[u8; 16], VaultError> {
    let mut salt = [0u8; 16];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| VaultError::Crypto)?;
    Ok(salt)
}

pub struct Vault {
    conn: Mutex<Connection>,
    key: Mutex<LessSafeKey>,
}

impl Vault {
    pub fn open(path: &str, passphrase: &str) -> Result<Self, VaultError> {
        Self::open_with_params(path, passphrase, KdfParams::default())
    }

    /// `params` solo se usa al crear un vault nuevo; uno existente usa los suyos.
    pub fn open_with_params(
        path: &str,
        passphrase: &str,
        params: KdfParams,
    ) -> Result<Self, VaultError> {
        if passphrase.chars().count() < PASSPHRASE_MIN {
            return Err(VaultError::Invalid(format!(
                "la passphrase debe tener al menos {PASSPHRASE_MIN} caracteres"
            )));
        }
        params.check().map_err(VaultError::Invalid)?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let has_meta: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='vault_meta'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();

        let key = if has_meta {
            Self::unlock(&conn, passphrase)?
        } else {
            Self::initialize(&conn, passphrase, params)?
        };
        Ok(Self {
            conn: Mutex::new(conn),
            key: Mutex::new(key),
        })
    }

    fn initialize(
        conn: &Connection,
        passphrase: &str,
        p: KdfParams,
    ) -> Result<LessSafeKey, VaultError> {
        let legacy_rows: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='secrets'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .map(|_| conn.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)))
            .transpose()?;
        match legacy_rows {
            Some(n) if n > 0 => return Err(VaultError::Legacy),
            Some(_) => conn.execute_batch("DROP TABLE secrets")?,
            None => {}
        }

        let salt = random_salt()?;
        let key = derive_key(passphrase, &salt, p)?;
        let canary = seal(&key, b"canary", CANARY)?;
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE vault_meta (k TEXT PRIMARY KEY, v BLOB NOT NULL);
             CREATE TABLE secrets (
                 name       TEXT PRIMARY KEY,
                 value      TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
        )?;
        let meta: [(&str, rusqlite::types::Value); 6] = [
            ("version", FORMAT_VERSION.into()),
            ("salt", salt.to_vec().into()),
            ("m_cost_kib", i64::from(p.m_cost_kib).into()),
            ("t_cost", i64::from(p.t_cost).into()),
            ("p_cost", i64::from(p.p_cost).into()),
            ("canary", canary.into()),
        ];
        for (k, v) in meta {
            conn.execute(
                "INSERT INTO vault_meta (k, v) VALUES (?1, ?2)",
                params![k, v],
            )?;
        }
        conn.execute_batch("COMMIT")?;
        Ok(key)
    }

    fn meta<T: rusqlite::types::FromSql>(conn: &Connection, k: &str) -> Result<T, VaultError> {
        conn.query_row("SELECT v FROM vault_meta WHERE k = ?1", params![k], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| VaultError::Db(format!("falta '{k}' en vault_meta")))
    }

    fn stored_params(conn: &Connection) -> Result<KdfParams, VaultError> {
        let get = |k| -> Result<u32, VaultError> {
            let v: i64 = Self::meta(conn, k)?;
            u32::try_from(v).map_err(|_| VaultError::Db(format!("'{k}' fuera de rango")))
        };
        let p = KdfParams {
            m_cost_kib: get("m_cost_kib")?,
            t_cost: get("t_cost")?,
            p_cost: get("p_cost")?,
        };
        p.check()
            .map_err(|e| VaultError::Db(format!("parámetros KDF inválidos en la base: {e}")))?;
        Ok(p)
    }

    /// Comprueba que la clave en memoria sigue siendo la del archivo (otro
    /// proceso podría haber cambiado la passphrase).
    fn key_is_current(conn: &Connection, key: &LessSafeKey) -> Result<bool, VaultError> {
        let canary: String = Self::meta(conn, "canary")?;
        Ok(matches!(open(key, b"canary", &canary), Ok(v) if v == CANARY))
    }

    fn unlock(conn: &Connection, passphrase: &str) -> Result<LessSafeKey, VaultError> {
        let version: i64 = Self::meta(conn, "version")?;
        if version != FORMAT_VERSION {
            return Err(VaultError::Db(format!(
                "versión de formato no soportada: {version}"
            )));
        }
        let salt: Vec<u8> = Self::meta(conn, "salt")?;
        let canary: String = Self::meta(conn, "canary")?;
        let key = derive_key(passphrase, &salt, Self::stored_params(conn)?)?;
        match open(&key, b"canary", &canary) {
            Ok(v) if v == CANARY => Ok(key),
            _ => Err(VaultError::WrongPassphrase),
        }
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn k(&self) -> MutexGuard<'_, LessSafeKey> {
        self.key.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Crea o actualiza un secreto.
    pub fn set_secret(&self, name: &str, value: &str) -> Result<(), VaultError> {
        validate_name(name)?;
        if value.len() > VALUE_MAX {
            return Err(VaultError::Invalid(format!(
                "el secreto supera {VALUE_MAX} bytes"
            )));
        }
        // Orden de bloqueo único en todo el módulo: conexión y después clave,
        // sostenidos hasta escribir. Así una rotación de passphrase no puede
        // colarse entre el cifrado y la escritura.
        let conn = self.c();
        let key = self.k();
        if !Self::key_is_current(&conn, &key)? {
            return Err(VaultError::Stale);
        }
        let ct = seal(&key, &secret_aad(name), value.as_bytes())?;
        let ts = now();
        conn.execute(
            "INSERT INTO secrets (name, value, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![name, ct, ts],
        )?;
        Ok(())
    }

    pub fn get_secret(&self, name: &str) -> Result<String, VaultError> {
        validate_name(name)?;
        let conn = self.c();
        let ct: String = conn
            .query_row(
                "SELECT value FROM secrets WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(VaultError::NotFound)?;
        let key = self.k();
        match open(&key, &secret_aad(name), &ct) {
            Ok(plain) => String::from_utf8(plain).map_err(|_| VaultError::Crypto),
            Err(_) if !Self::key_is_current(&conn, &key)? => Err(VaultError::Stale),
            Err(e) => Err(e),
        }
    }

    pub fn list_secrets(&self) -> Result<Vec<String>, VaultError> {
        let c = self.c();
        let mut st = c.prepare("SELECT name FROM secrets ORDER BY name")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Devuelve `true` si existía.
    pub fn delete_secret(&self, name: &str) -> Result<bool, VaultError> {
        validate_name(name)?;
        Ok(self
            .c()
            .execute("DELETE FROM secrets WHERE name = ?1", params![name])?
            == 1)
    }

    /// Cifra datos arbitrarios con la clave del vault. `context` se autentica:
    /// para descifrar hay que dar el mismo.
    ///
    /// Atención: estos tokens viven fuera del vault, así que
    /// [`Vault::change_passphrase`] no puede re-cifrarlos. Descífrelos antes
    /// de rotar la passphrase o dejarán de poder leerse.
    pub fn encrypt(&self, data: &[u8], context: &str) -> Result<String, VaultError> {
        seal(&self.k(), &data_aad(context), data)
    }

    pub fn decrypt(&self, token: &str, context: &str) -> Result<Vec<u8>, VaultError> {
        open(&self.k(), &data_aad(context), token)
    }

    /// Cambia la passphrase: nueva sal, nueva clave y re-cifrado de todos los
    /// secretos en una sola transacción. Otros handles abiertos sobre el mismo
    /// archivo detectan el cambio y devuelven [`VaultError::Stale`] en vez de
    /// escribir con la clave vieja. Los tokens de [`Vault::encrypt`] no se
    /// re-cifran.
    pub fn change_passphrase(&self, new_passphrase: &str) -> Result<(), VaultError> {
        if new_passphrase.chars().count() < PASSPHRASE_MIN {
            return Err(VaultError::Invalid(format!(
                "la passphrase debe tener al menos {PASSPHRASE_MIN} caracteres"
            )));
        }
        let mut conn = self.c();
        let mut key = self.k();
        let params = Self::stored_params(&conn)?;
        let salt = random_salt()?;
        let new_key = derive_key(new_passphrase, &salt, params)?;

        let tx = conn.transaction()?;
        let rows: Vec<(String, String)> = {
            let mut st = tx.prepare("SELECT name, value FROM secrets")?;
            let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            it.collect::<Result<_, _>>()?
        };
        for (name, ct) in rows {
            let plain = Zeroizing::new(open(&key, &secret_aad(&name), &ct)?);
            let re = seal(&new_key, &secret_aad(&name), &plain)?;
            tx.execute(
                "UPDATE secrets SET value = ?2 WHERE name = ?1",
                params![name, re],
            )?;
        }
        tx.execute(
            "UPDATE vault_meta SET v = ?1 WHERE k = 'salt'",
            params![salt.to_vec()],
        )?;
        tx.execute(
            "UPDATE vault_meta SET v = ?1 WHERE k = 'canary'",
            params![seal(&new_key, b"canary", CANARY)?],
        )?;
        tx.commit()?;
        *key = new_key;
        Ok(())
    }
}

/// Certificado autofirmado ECDSA P-256. Devuelve `(cert_pem, private_key_pem)`.
pub fn generate_certificate(common_name: &str, days: u32) -> Result<(String, String), VaultError> {
    let cn = common_name.trim();
    let valid = !cn.is_empty()
        && cn.len() <= 253
        && cn
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'*'));
    if !valid {
        return Err(VaultError::Invalid(
            "common_name debe ser un nombre DNS válido".into(),
        ));
    }
    if !(1..=825).contains(&days) {
        return Err(VaultError::Invalid("days debe estar entre 1 y 825".into()));
    }
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|_| VaultError::Crypto)?;
    let mut params = rcgen::CertificateParams::new(vec![cn.to_owned()])
        .map_err(|e| VaultError::Invalid(format!("common_name inválido: {e}")))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    // Basado en segundos desde ahora: no falla el 29 de febrero como antes.
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(i64::from(days));
    let cert = params
        .self_signed(&key_pair)
        .map_err(|_| VaultError::Crypto)?;
    Ok((cert.pem(), key_pair.serialize_pem()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Parámetros mínimos para que los tests sean rápidos.
    const FAST: KdfParams = KdfParams {
        m_cost_kib: 8,
        t_cost: 1,
        p_cost: 1,
    };
    const PASS: &str = "correct horse battery staple";

    fn tmp() -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("v.db").to_str().unwrap().to_owned();
        (d, p)
    }

    #[test]
    fn secrets_survive_reopen() {
        let (_d, p) = tmp();
        Vault::open_with_params(&p, PASS, FAST)
            .unwrap()
            .set_secret("api_key", "s3cr3t")
            .unwrap();
        let v = Vault::open(&p, PASS).unwrap();
        assert_eq!(v.get_secret("api_key").unwrap(), "s3cr3t");
    }

    #[test]
    fn wrong_passphrase_is_detected() {
        let (_d, p) = tmp();
        Vault::open_with_params(&p, PASS, FAST).unwrap();
        assert_eq!(
            Vault::open(&p, "not the passphrase").err(),
            Some(VaultError::WrongPassphrase)
        );
    }

    #[test]
    fn short_passphrase_rejected() {
        let (_d, p) = tmp();
        assert!(matches!(
            Vault::open(&p, "short"),
            Err(VaultError::Invalid(_))
        ));
    }

    #[test]
    fn ciphertext_is_bound_to_its_name() {
        let (_d, p) = tmp();
        let v = Vault::open_with_params(&p, PASS, FAST).unwrap();
        v.set_secret("a", "alpha").unwrap();
        v.set_secret("b", "beta").unwrap();
        {
            let c = v.c();
            c.execute("UPDATE secrets SET value = (SELECT value FROM secrets WHERE name='a') WHERE name='b'", [])
                .unwrap();
        }
        assert_eq!(v.get_secret("b"), Err(VaultError::Crypto));
    }

    #[test]
    fn upsert_list_delete() {
        let (_d, p) = tmp();
        let v = Vault::open_with_params(&p, PASS, FAST).unwrap();
        v.set_secret("x", "1").unwrap();
        v.set_secret("x", "2").unwrap();
        assert_eq!(v.get_secret("x").unwrap(), "2");
        assert_eq!(v.list_secrets().unwrap(), vec!["x"]);
        assert!(v.delete_secret("x").unwrap());
        assert!(!v.delete_secret("x").unwrap());
        assert_eq!(v.get_secret("x"), Err(VaultError::NotFound));
    }

    #[test]
    fn invalid_names_rejected() {
        assert!(validate_name("db/password").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("with space").is_err());
        assert!(validate_name(&"a".repeat(129)).is_err());
    }

    #[test]
    fn encrypt_decrypt_with_context() {
        let (_d, p) = tmp();
        let v = Vault::open_with_params(&p, PASS, FAST).unwrap();
        let t = v.encrypt(b"datos", "ctx").unwrap();
        assert_eq!(v.decrypt(&t, "ctx").unwrap(), b"datos");
        assert_eq!(v.decrypt(&t, "otro"), Err(VaultError::Crypto));
        assert_eq!(v.decrypt("!!!", "ctx"), Err(VaultError::Crypto));
    }

    #[test]
    fn change_passphrase_reencrypts_everything() {
        let (_d, p) = tmp();
        let v = Vault::open_with_params(&p, PASS, FAST).unwrap();
        v.set_secret("k", "valor").unwrap();
        v.change_passphrase("a brand new passphrase").unwrap();
        assert_eq!(v.get_secret("k").unwrap(), "valor");
        drop(v);
        assert_eq!(
            Vault::open(&p, PASS).err(),
            Some(VaultError::WrongPassphrase)
        );
        let v = Vault::open(&p, "a brand new passphrase").unwrap();
        assert_eq!(v.get_secret("k").unwrap(), "valor");
    }

    #[test]
    fn second_handle_detects_rotation_instead_of_losing_data() {
        let (_d, p) = tmp();
        let a = Vault::open_with_params(&p, PASS, FAST).unwrap();
        let b = Vault::open(&p, PASS).unwrap();
        a.set_secret("k", "v").unwrap();
        a.change_passphrase("a brand new passphrase").unwrap();
        assert_eq!(b.set_secret("x", "1"), Err(VaultError::Stale));
        assert_eq!(b.get_secret("k"), Err(VaultError::Stale));
        assert_eq!(a.get_secret("k").unwrap(), "v");
    }

    #[test]
    fn kdf_params_are_bounded() {
        let (_d, p) = tmp();
        let huge = KdfParams {
            m_cost_kib: u32::MAX,
            t_cost: 3,
            p_cost: 1,
        };
        assert!(matches!(
            Vault::open_with_params(&p, PASS, huge),
            Err(VaultError::Invalid(_))
        ));
        Vault::open_with_params(&p, PASS, FAST).unwrap();
        {
            let c = Connection::open(&p).unwrap();
            c.execute(
                "UPDATE vault_meta SET v = 4000000000 WHERE k = 'm_cost_kib'",
                [],
            )
            .unwrap();
        }
        assert!(matches!(Vault::open(&p, PASS), Err(VaultError::Db(_))));
    }

    #[test]
    fn legacy_v01_database_is_refused_not_overwritten() {
        let (_d, p) = tmp();
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(
                "CREATE TABLE secrets (name TEXT PRIMARY KEY, value TEXT NOT NULL, created_at TEXT NOT NULL);
                 INSERT INTO secrets VALUES ('api_key', 'xxxx', '2024-10-12');",
            )
            .unwrap();
        }
        assert_eq!(
            Vault::open_with_params(&p, PASS, FAST).err(),
            Some(VaultError::Legacy)
        );
        let c = Connection::open(&p).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "los datos antiguos no se tocan");
    }

    #[test]
    fn certificate_comes_with_its_private_key() {
        let (cert, key) = generate_certificate("example.com", 30).unwrap();
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(key.contains("PRIVATE KEY"));
        assert!(generate_certificate("bad name!", 30).is_err());
        assert!(generate_certificate("example.com", 0).is_err());
    }
}
