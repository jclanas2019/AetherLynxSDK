use std::time::{SystemTime, UNIX_EPOCH};

pub struct AuditLog {
    pub logs: Vec<String>,
}

impl AuditLog {
    pub fn new() -> AuditLog {
        AuditLog {
            logs: Vec::new(),
        }
    }

    pub fn log_event(&mut self, username: &str, resource: &str, success: bool) {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let status = if success { "Éxito" } else { "Fallo" };
        let log = format!("{}: Usuario {} accedió a {} con {}", timestamp, username, resource, status);
        self.logs.push(log);
    }

    pub fn show_logs(&self) {
        for log in &self.logs {
            println!("{}", log);
        }
    }
}
