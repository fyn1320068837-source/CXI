// Storage - SQLite + Keychain integration
use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct InstanceState {
    pub id: i64,
    pub kind: String,
    pub status: String,
    pub root_pid: Option<u32>,
    pub started_at: Option<String>,
    pub stopped_at: Option<String>,
}

pub struct Storage {
    conn: Mutex<Connection>,
}

impl Storage {
    pub fn new(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        let storage = Self {
            conn: Mutex::new(conn),
        };
        storage.initialize_schema()?;
        Ok(storage)
    }

    fn initialize_schema(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS proxy_profile (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                host TEXT NOT NULL,
                port INTEGER NOT NULL,
                username TEXT,
                keychain_ref TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS instance_state (
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                root_pid INTEGER,
                started_at TEXT,
                stopped_at TEXT
            );

            CREATE TABLE IF NOT EXISTS validation_evidence (
                id INTEGER PRIMARY KEY,
                instance_id INTEGER NOT NULL,
                timestamp TEXT NOT NULL,
                status TEXT NOT NULL,
                proxied_count INTEGER NOT NULL,
                local_count INTEGER NOT NULL,
                direct_count INTEGER NOT NULL,
                details TEXT,
                FOREIGN KEY(instance_id) REFERENCES instance_state(id)
            );

            CREATE TABLE IF NOT EXISTS exit_ip_log (
                id INTEGER PRIMARY KEY,
                timestamp TEXT NOT NULL,
                ip_address TEXT NOT NULL,
                country TEXT,
                city TEXT,
                asn TEXT
            );

            CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY,
                timestamp TEXT NOT NULL,
                event_type TEXT NOT NULL,
                actor TEXT,
                details TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn save_proxy_profile(
        &self,
        name: &str,
        host: &str,
        port: u16,
        username: Option<&str>,
        keychain_ref: Option<&str>,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();

        conn.execute(
            "INSERT INTO proxy_profile (name, host, port, username, keychain_ref, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![name, host, port, username, keychain_ref, &now, &now],
        )?;

        Ok(conn.last_insert_rowid())
    }

    #[allow(dead_code)]
    pub fn log_audit_event(
        &self,
        event_type: &str,
        actor: Option<&str>,
        details: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();

        conn.execute(
            "INSERT INTO audit_log (timestamp, event_type, actor, details) VALUES (?1, ?2, ?3, ?4)",
            params![&now, event_type, actor, details],
        )?;

        Ok(())
    }

    pub fn save_instance_state(
        &self,
        kind: &str,
        status: &str,
        root_pid: Option<u32>,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let (started_at, stopped_at) = if status == "running" {
            (Some(now.clone()), None)
        } else {
            (None, Some(now))
        };

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO instance_state (kind, status, root_pid, started_at, stopped_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, status, root_pid, started_at, stopped_at],
        )?;

        Ok(conn.last_insert_rowid())
    }

    #[allow(dead_code)]
    pub fn get_instance_state(&self, kind: &str) -> Result<Option<InstanceState>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, status, root_pid, started_at, stopped_at FROM instance_state WHERE kind = ?1 ORDER BY id DESC LIMIT 1"
        )?;

        let result = stmt.query_row(params![kind], |row| {
            Ok(InstanceState {
                id: row.get(0)?,
                kind: row.get(1)?,
                status: row.get(2)?,
                root_pid: row.get(3)?,
                started_at: row.get(4)?,
                stopped_at: row.get(5)?,
            })
        });

        match result {
            Ok(state) => Ok(Some(state)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

// Keychain integration (macOS only)
#[cfg(target_os = "macos")]
pub mod keychain {
    use anyhow::Result;
    use security_framework::passwords::*;

    pub fn save_password(service: &str, account: &str, password: &str) -> Result<()> {
        set_generic_password(service, account, password.as_bytes())?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_password(service: &str, account: &str) -> Result<String> {
        let password_bytes = get_generic_password(service, account)?;
        let password = String::from_utf8(password_bytes)?;
        Ok(password)
    }

    #[allow(dead_code)]
    pub fn delete_password(_service: &str, _account: &str) -> Result<()> {
        // TODO: Implement deletion
        Ok(())
    }
}
