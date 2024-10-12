use rusqlite::{Connection, Result};
use std::path::Path;
use std::fs;

pub struct DBManager {
    conn: Connection,
}

impl DBManager {
    pub fn new() -> Result<Self> {
        let config = fs::read_to_string("config.toml")
            .expect("Unable to read config file");
        let db_path = config.lines()
            .find(|line| line.starts_with("database_path"))
            .and_then(|line| line.split('=').nth(1))
            .map(|path| path.trim().trim_matches('"'))
            .expect("Unable to find database_path in config");

        let db_exists = Path::new(db_path).exists();
        let conn = Connection::open(db_path)?;
        let manager = DBManager { conn };

        if !db_exists {
            manager.init_db()?;
        }

        Ok(manager)
    }

    fn init_db(&self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS users (
                username TEXT PRIMARY KEY,
                password TEXT NOT NULL
            )",
            [],
        )?;
        Ok(())
    }

    pub fn store_user(&self, username: &str, password: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO users (username, password) VALUES (?1, ?2)",
            [username, password],
        )?;
        Ok(())
    }

    pub fn get_user(&self, username: &str) -> Result<Option<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT username, password FROM users WHERE username = ?1")?;
        let mut rows = stmt.query([username])?;
        
        if let Some(row) = rows.next()? {
            Ok(Some((row.get(0)?, row.get(1)?)))
        } else {
            Ok(None)
        }
    }
}