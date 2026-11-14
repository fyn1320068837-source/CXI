// Storage - SQLite + Keychain integration
use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

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

#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Serialize)]
pub struct ValidationEvidence {
    pub timestamp: String,
    pub status: String,
    pub proxied_count: usize,
    pub local_count: usize,
    pub direct_count: usize,
    pub details: Option<String>,
}

impl Storage {
    pub fn new(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        let storage = Self {
            conn: Arc::new(Mutex::new(conn)),
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

    /// Run the synchronous SQLite write away from the Tokio worker thread.
    pub async fn save_instance_state_async(
        &self,
        kind: &str,
        status: &str,
        root_pid: Option<u32>,
    ) -> Result<i64> {
        let storage = self.clone();
        let kind = kind.to_owned();
        let status = status.to_owned();
        tokio::task::spawn_blocking(move || storage.save_instance_state(&kind, &status, root_pid))
            .await?
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

    /// Persist one process-level network validation result for the most recent
    /// instance state of the given kind. The evidence is intentionally a
    /// summary: direct endpoints are already included in `details`, while the
    /// numeric counters make the result easy to query and aggregate.
    /// Run the synchronous SQLite evidence write off the async executor.
    pub async fn save_validation_evidence_async(
        &self,
        kind: &str,
        status: &str,
        proxied_count: usize,
        local_count: usize,
        direct_count: usize,
        details: &str,
    ) -> Result<()> {
        let storage = self.clone();
        let kind = kind.to_owned();
        let status = status.to_owned();
        let details = details.to_owned();
        tokio::task::spawn_blocking(move || {
            storage.save_validation_evidence(
                &kind,
                &status,
                proxied_count,
                local_count,
                direct_count,
                &details,
            )
        })
        .await?
    }

    pub fn save_validation_evidence(
        &self,
        kind: &str,
        status: &str,
        proxied_count: usize,
        local_count: usize,
        direct_count: usize,
        details: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO validation_evidence
                (instance_id, timestamp, status, proxied_count, local_count, direct_count, details)
             SELECT id, ?2, ?3, ?4, ?5, ?6, ?7
             FROM instance_state
             WHERE kind = ?1
             ORDER BY id DESC
             LIMIT 1",
            params![
                kind,
                &now,
                status,
                proxied_count as i64,
                local_count as i64,
                direct_count as i64,
                details,
            ],
        )?;
        // Status polling is intentionally frequent so a leak is detected
        // quickly. Keep a bounded history per instance rather than allowing
        // the evidence table to grow forever.
        conn.execute(
            "DELETE FROM validation_evidence
             WHERE id IN (
                 SELECT ve.id
                 FROM validation_evidence ve
                 JOIN instance_state state ON state.id = ve.instance_id
                 WHERE state.kind = ?1
                 ORDER BY ve.id DESC
                 LIMIT -1 OFFSET 500
             )",
            params![kind],
        )?;
        Ok(())
    }

    /// Read recent evidence without holding a SQLite mutex on an async task.
    pub async fn recent_validation_evidence_async(
        &self,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<ValidationEvidence>> {
        let storage = self.clone();
        let kind = kind.to_owned();
        tokio::task::spawn_blocking(move || storage.recent_validation_evidence(&kind, limit))
            .await?
    }

    pub fn recent_validation_evidence(
        &self,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<ValidationEvidence>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT ve.timestamp, ve.status, ve.proxied_count, ve.local_count,
                    ve.direct_count, ve.details
             FROM validation_evidence ve
             JOIN instance_state state ON state.id = ve.instance_id
             WHERE state.kind = ?1
             ORDER BY ve.id DESC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![kind, limit as i64], |row| {
            Ok(ValidationEvidence {
                timestamp: row.get(0)?,
                status: row.get(1)?,
                proxied_count: row.get::<_, i64>(2)?.max(0) as usize,
                local_count: row.get::<_, i64>(3)?.max(0) as usize,
                direct_count: row.get::<_, i64>(4)?.max(0) as usize,
                details: row.get(5)?,
            })
        })?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    #[allow(dead_code)]
    /// Read the latest state without blocking the Tokio executor.
    pub async fn get_instance_state_async(&self, kind: &str) -> Result<Option<InstanceState>> {
        let storage = self.clone();
        let kind = kind.to_owned();
        tokio::task::spawn_blocking(move || storage.get_instance_state(&kind)).await?
    }

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

#[cfg(test)]
mod tests {
    use super::Storage;

    #[tokio::test]
    async fn async_storage_wrappers_round_trip_without_blocking_callers() {
        let path =
            std::env::temp_dir().join(format!("cim-storage-async-{}.db", uuid::Uuid::new_v4()));
        let storage = Storage::new(&path).expect("temporary database should open");

        storage
            .save_instance_state_async("web", "running", Some(4242))
            .await
            .unwrap();
        let state = storage
            .get_instance_state_async("web")
            .await
            .unwrap()
            .expect("saved instance state should be readable");
        assert_eq!(state.root_pid, Some(4242));

        storage
            .save_validation_evidence_async("web", "pass", 2, 1, 0, "proxied=2")
            .await
            .unwrap();
        let evidence = storage
            .recent_validation_evidence_async("web", 1)
            .await
            .unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].proxied_count, 2);

        drop(storage);
        let _ = std::fs::remove_file(path);
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
