//! Almacén SQLite compartido por autenticación, roles y auditoría.

use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub username: String,
    pub password_hash: String,
    pub failed_attempts: u32,
    pub locked_until: i64,
    /// Identificador aleatorio de la cuenta: cambia si la cuenta se borra y
    /// se vuelve a crear con el mismo nombre.
    pub account_id: String,
    /// Se incrementa al cambiar la contraseña o revocar sesiones.
    pub token_version: i64,
}

#[derive(Debug, Clone)]
pub struct AuditRow {
    pub ts: i64,
    pub username: String,
    pub action: String,
    pub success: bool,
    pub detail: String,
}

pub type DbResult<T> = Result<T, rusqlite::Error>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    username        TEXT PRIMARY KEY,
    password_hash   TEXT NOT NULL,
    failed_attempts INTEGER NOT NULL DEFAULT 0,
    locked_until    INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    account_id      TEXT NOT NULL DEFAULT '',
    token_version   INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS user_roles (
    username TEXT NOT NULL REFERENCES users(username) ON DELETE CASCADE,
    role     TEXT NOT NULL,
    PRIMARY KEY (username, role)
);
CREATE TABLE IF NOT EXISTS resource_roles (
    resource TEXT NOT NULL,
    role     TEXT NOT NULL,
    PRIMARY KEY (resource, role)
);
CREATE TABLE IF NOT EXISTS audit_log (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    ts       INTEGER NOT NULL,
    username TEXT NOT NULL,
    action   TEXT NOT NULL,
    success  INTEGER NOT NULL,
    detail   TEXT NOT NULL DEFAULT ''
);
";

fn columns(conn: &Connection, table: &str) -> DbResult<Vec<String>> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = st.query_map([], |r| r.get::<_, String>(1))?;
    cols.collect()
}

/// Lleva una tabla `users` antigua (versión 0.1: `username`, `password`) al
/// esquema actual sin perder cuentas. Los hashes de la 0.1 ya eran Argon2,
/// así que siguen siendo verificables.
fn migrate_users(conn: &Connection) -> DbResult<()> {
    let cols = columns(conn, "users")?;
    if cols.is_empty() {
        return Ok(()); // base nueva: la crea SCHEMA
    }
    let has = |c: &str| cols.iter().any(|x| x == c);
    let tx = conn.unchecked_transaction()?;
    if has("password") && !has("password_hash") {
        tx.execute_batch("ALTER TABLE users RENAME COLUMN password TO password_hash")?;
    }
    for (col, ddl) in [
        ("failed_attempts", "INTEGER NOT NULL DEFAULT 0"),
        ("locked_until", "INTEGER NOT NULL DEFAULT 0"),
        ("created_at", "INTEGER NOT NULL DEFAULT 0"),
        ("account_id", "TEXT NOT NULL DEFAULT ''"),
        ("token_version", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !has(col) {
            tx.execute_batch(&format!("ALTER TABLE users ADD COLUMN {col} {ddl}"))?;
        }
    }
    tx.execute_batch(
        "UPDATE users SET account_id = lower(hex(randomblob(16))) WHERE account_id = ''",
    )?;
    // La 0.2 normaliza los nombres a minúsculas. Solo se convierten los que
    // no chocan con otro nombre igual salvo mayúsculas ("Bob" y "BOB").
    tx.execute_batch(
        "UPDATE users SET username = lower(username)
         WHERE username <> lower(username)
           AND lower(username) IN (
               SELECT lower(username) FROM users GROUP BY lower(username) HAVING COUNT(*) = 1)",
    )?;
    let collisions: i64 = tx.query_row(
        "SELECT COUNT(*) FROM users WHERE username <> lower(username)",
        [],
        |r| r.get(0),
    )?;
    if collisions > 0 {
        eprintln!(
            "pam_ipa_project: {collisions} cuenta(s) no se pasaron a minúsculas porque chocan con otra \
             (p. ej. 'Bob' y 'BOB'). No podrán iniciar sesión hasta que un administrador las renombre: \
             SELECT username FROM users WHERE username <> lower(username);"
        );
    }
    tx.commit()
}

impl Store {
    pub fn open(path: &str) -> DbResult<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // WAL permite que auth e IPA compartan el archivo con lectores concurrentes.
        if path != ":memory:" {
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate_users(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- usuarios -------------------------------------------------------

    /// Inserta un usuario nuevo. Devuelve `false` si ya existía (nunca lo sobrescribe).
    pub fn insert_user(
        &self,
        username: &str,
        hash: &str,
        now: i64,
        account_id: &str,
    ) -> DbResult<bool> {
        let n = self.c().execute(
            "INSERT OR IGNORE INTO users (username, password_hash, created_at, account_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![username, hash, now, account_id],
        )?;
        Ok(n == 1)
    }

    pub fn get_user(&self, username: &str) -> DbResult<Option<UserRow>> {
        self.c()
            .query_row(
                "SELECT username, password_hash, failed_attempts, locked_until, account_id, token_version
                 FROM users WHERE username = ?1",
                params![username],
                |r| {
                    Ok(UserRow {
                        username: r.get(0)?,
                        password_hash: r.get(1)?,
                        failed_attempts: r.get(2)?,
                        locked_until: r.get(3)?,
                        account_id: r.get(4)?,
                        token_version: r.get(5)?,
                    })
                },
            )
            .optional()
    }

    pub fn update_password(&self, username: &str, hash: &str) -> DbResult<()> {
        self.c().execute(
            "UPDATE users SET password_hash = ?2, failed_attempts = 0, locked_until = 0,
                 token_version = token_version + 1 WHERE username = ?1",
            params![username, hash],
        )?;
        Ok(())
    }

    /// Reserva un intento de login de forma atómica, ANTES de verificar la
    /// contraseña: suma 1 al contador y bloquea la cuenta al llegar al máximo.
    /// Así, peticiones en paralelo no pueden pasar todas el control de
    /// bloqueo antes de que se registre ningún fallo. Un bloqueo vencido
    /// reinicia el contador. Devuelve el nuevo contador, o `None` si la cuenta
    /// está bloqueada (o no existe). Un login correcto lo pone a cero después.
    pub fn reserve_attempt(
        &self,
        username: &str,
        now: i64,
        max_attempts: u32,
        lock_until: i64,
    ) -> DbResult<Option<u32>> {
        self.c()
            .query_row(
                "UPDATE users SET
                     failed_attempts = CASE WHEN locked_until <> 0 THEN 1 ELSE failed_attempts + 1 END,
                     locked_until = CASE
                         WHEN (CASE WHEN locked_until <> 0 THEN 1 ELSE failed_attempts + 1 END) >= ?3 THEN ?4
                         ELSE 0 END
                 WHERE username = ?1 AND locked_until <= ?2
                 RETURNING failed_attempts",
                params![username, now, max_attempts, lock_until],
                |r| r.get(0),
            )
            .optional()
    }

    /// Invalida todos los tokens emitidos para la cuenta.
    pub fn bump_token_version(&self, username: &str) -> DbResult<bool> {
        Ok(self.c().execute(
            "UPDATE users SET token_version = token_version + 1 WHERE username = ?1",
            params![username],
        )? == 1)
    }

    pub fn reset_failures(&self, username: &str) -> DbResult<()> {
        self.c().execute(
            "UPDATE users SET failed_attempts = 0, locked_until = 0 WHERE username = ?1",
            params![username],
        )?;
        Ok(())
    }

    pub fn delete_user(&self, username: &str) -> DbResult<bool> {
        Ok(self
            .c()
            .execute("DELETE FROM users WHERE username = ?1", params![username])?
            == 1)
    }

    pub fn user_exists(&self, username: &str) -> DbResult<bool> {
        Ok(self
            .c()
            .query_row(
                "SELECT 1 FROM users WHERE username = ?1",
                params![username],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    // ---- roles y recursos -----------------------------------------------

    pub fn add_role(&self, username: &str, role: &str) -> DbResult<()> {
        self.c().execute(
            "INSERT OR IGNORE INTO user_roles (username, role) VALUES (?1, ?2)",
            params![username, role],
        )?;
        Ok(())
    }

    pub fn remove_role(&self, username: &str, role: &str) -> DbResult<()> {
        self.c().execute(
            "DELETE FROM user_roles WHERE username = ?1 AND role = ?2",
            params![username, role],
        )?;
        Ok(())
    }

    pub fn roles(&self, username: &str) -> DbResult<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT role FROM user_roles WHERE username = ?1 ORDER BY role")?;
        let rows = st.query_map(params![username], |r| r.get(0))?;
        rows.collect()
    }

    pub fn add_resource(&self, resource: &str, role: &str) -> DbResult<()> {
        self.c().execute(
            "INSERT OR IGNORE INTO resource_roles (resource, role) VALUES (?1, ?2)",
            params![resource, role],
        )?;
        Ok(())
    }

    pub fn remove_resource(&self, resource: &str) -> DbResult<()> {
        self.c().execute(
            "DELETE FROM resource_roles WHERE resource = ?1",
            params![resource],
        )?;
        Ok(())
    }

    pub fn has_access(&self, username: &str, resource: &str) -> DbResult<bool> {
        Ok(self
            .c()
            .query_row(
                "SELECT 1 FROM user_roles ur JOIN resource_roles rr ON ur.role = rr.role
                 WHERE ur.username = ?1 AND rr.resource = ?2 LIMIT 1",
                params![username, resource],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn accessible_resources(&self, username: &str) -> DbResult<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT DISTINCT rr.resource FROM user_roles ur JOIN resource_roles rr ON ur.role = rr.role
             WHERE ur.username = ?1 ORDER BY rr.resource",
        )?;
        let rows = st.query_map(params![username], |r| r.get(0))?;
        rows.collect()
    }

    // ---- auditoría ------------------------------------------------------

    pub fn audit(
        &self,
        ts: i64,
        username: &str,
        action: &str,
        success: bool,
        detail: &str,
    ) -> DbResult<()> {
        self.c().execute(
            "INSERT INTO audit_log (ts, username, action, success, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ts, username, action, success, detail],
        )?;
        Ok(())
    }

    pub fn audit_events(&self, limit: u32) -> DbResult<Vec<AuditRow>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT ts, username, action, success, detail FROM audit_log ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = st.query_map(params![limit], |r| {
            Ok(AuditRow {
                ts: r.get(0)?,
                username: r.get(1)?,
                action: r.get(2)?,
                success: r.get(3)?,
                detail: r.get(4)?,
            })
        })?;
        rows.collect()
    }
}
