//! The durable job and file-instance ledger.
//!
//! Content jobs remain keyed by the true SHA-256 so expensive conversion and
//! naming work can be reused. Physical file instances are tracked separately so
//! identical bytes at different paths still receive replay-safe manifests and
//! distinct reserved filenames.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Ingested,
    Converted,
    Filtered,
    Named,
    Validated,
    Emitted,
    Flagged,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Ingested => "ingested",
            JobState::Converted => "converted",
            JobState::Filtered => "filtered",
            JobState::Named => "named",
            JobState::Validated => "validated",
            JobState::Emitted => "emitted",
            JobState::Flagged => "flagged",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ingested" => JobState::Ingested,
            "converted" => JobState::Converted,
            "filtered" => JobState::Filtered,
            "named" => JobState::Named,
            "validated" => JobState::Validated,
            "emitted" => JobState::Emitted,
            "flagged" => JobState::Flagged,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Discovered,
    Processing,
    Emitted,
    Flagged,
}

impl InstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            InstanceState::Discovered => "discovered",
            InstanceState::Processing => "processing",
            InstanceState::Emitted => "emitted",
            InstanceState::Flagged => "flagged",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "discovered" => InstanceState::Discovered,
            "processing" => InstanceState::Processing,
            "emitted" => InstanceState::Emitted,
            "flagged" => InstanceState::Flagged,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, InstanceState::Emitted | InstanceState::Flagged)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub sha256: String,
    pub original_path: String,
    pub original_name: String,
    pub ext: String,
    pub detected_type: String,
    pub route: String,
    pub state: JobState,
    pub attempts: u8,
    pub flag_reason: Option<String>,
    pub proposed_date: Option<String>,
    pub date_source: Option<String>,
    pub proposed_subject: Option<String>,
    pub description: Option<String>,
    pub final_filename: Option<String>,
    pub doc_type: Option<String>,
    pub language: Option<String>,
    pub duplicate_of: Option<String>,
    pub soft_flags: Option<String>,
    pub model_versions: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInstance {
    pub instance_id: String,
    pub sha256: String,
    pub original_path: String,
    pub original_name: String,
    pub ext: String,
    pub state: InstanceState,
    pub final_filename: Option<String>,
    pub manifest_id: String,
    pub created_at: String,
    pub updated_at: String,
}

pub struct Ledger {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS jobs (
    sha256          TEXT PRIMARY KEY,
    original_path   TEXT NOT NULL,
    original_name   TEXT NOT NULL,
    ext             TEXT NOT NULL,
    detected_type   TEXT NOT NULL DEFAULT '',
    route           TEXT NOT NULL DEFAULT '',
    state           TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    flag_reason     TEXT,
    proposed_date   TEXT,
    date_source     TEXT,
    proposed_subject TEXT,
    description     TEXT,
    final_filename  TEXT,
    doc_type        TEXT,
    language        TEXT,
    duplicate_of    TEXT,
    soft_flags      TEXT,
    model_versions  TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_jobs_state ON jobs(state);
CREATE INDEX IF NOT EXISTS idx_jobs_final ON jobs(final_filename);

CREATE TABLE IF NOT EXISTS file_instances (
    instance_id     TEXT PRIMARY KEY,
    sha256          TEXT NOT NULL,
    original_path   TEXT NOT NULL,
    original_name   TEXT NOT NULL,
    ext             TEXT NOT NULL,
    state           TEXT NOT NULL DEFAULT 'discovered',
    final_filename  TEXT,
    manifest_id     TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_instances_sha ON file_instances(sha256);
CREATE INDEX IF NOT EXISTS idx_instances_state ON file_instances(state);

CREATE TABLE IF NOT EXISTS filename_reservations (
    final_filename  TEXT COLLATE NOCASE PRIMARY KEY,
    instance_id     TEXT NOT NULL UNIQUE,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    FOREIGN KEY(instance_id) REFERENCES file_instances(instance_id)
);

CREATE TABLE IF NOT EXISTS events (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    sha256   TEXT NOT NULL,
    at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    stage    TEXT NOT NULL,
    detail   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_sha ON events(sha256);
"#;

impl Ledger {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Register one physical file instance. A replay returns the existing row.
    pub fn register_instance(
        &self,
        instance_id: &str,
        sha256: &str,
        original_path: &str,
        original_name: &str,
        ext: &str,
    ) -> anyhow::Result<Option<FileInstance>> {
        anyhow::ensure!(
            crate::identity::is_safe_identifier(instance_id),
            "unsafe instance identifier"
        );
        anyhow::ensure!(
            crate::identity::is_safe_identifier(sha256),
            "invalid content SHA-256"
        );

        let conn = self.conn.lock().unwrap();
        if let Some(existing) = Self::get_instance_inner(&conn, instance_id)? {
            anyhow::ensure!(
                existing.sha256 == sha256,
                "instance identifier collision for different content"
            );
            return Ok(Some(existing));
        }

        conn.execute(
            "INSERT INTO file_instances
             (instance_id, sha256, original_path, original_name, ext, state, manifest_id)
             VALUES (?1, ?2, ?3, ?4, ?5, 'discovered', ?1)",
            params![instance_id, sha256, original_path, original_name, ext],
        )?;
        Ok(None)
    }

    pub fn instance(&self, instance_id: &str) -> anyhow::Result<Option<FileInstance>> {
        let conn = self.conn.lock().unwrap();
        Self::get_instance_inner(&conn, instance_id)
    }

    fn get_instance_inner(
        conn: &Connection,
        instance_id: &str,
    ) -> anyhow::Result<Option<FileInstance>> {
        let instance = conn
            .query_row(
                "SELECT * FROM file_instances WHERE instance_id = ?1",
                params![instance_id],
                Self::row_to_instance,
            )
            .optional()?;
        Ok(instance)
    }

    fn row_to_instance(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileInstance> {
        Ok(FileInstance {
            instance_id: row.get("instance_id")?,
            sha256: row.get("sha256")?,
            original_path: row.get("original_path")?,
            original_name: row.get("original_name")?,
            ext: row.get("ext")?,
            state: InstanceState::parse(&row.get::<_, String>("state")?)
                .unwrap_or(InstanceState::Flagged),
            final_filename: row.get("final_filename")?,
            manifest_id: row.get("manifest_id")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    pub fn set_instance_state(
        &self,
        instance_id: &str,
        state: InstanceState,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE file_instances
             SET state=?2, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE instance_id=?1",
            params![instance_id, state.as_str()],
        )?;
        anyhow::ensure!(changed == 1, "unknown file instance {instance_id}");
        Ok(())
    }

    /// Reserve a complete filename atomically. Replaying the same instance
    /// always returns its existing reservation, regardless of a new proposal.
    pub fn reserve_filename(
        &self,
        instance_id: &str,
        base: &str,
        ext: &str,
    ) -> anyhow::Result<String> {
        let base = base.trim();
        anyhow::ensure!(!base.is_empty(), "cannot reserve an empty filename");
        let ext = ext.trim().trim_start_matches('.');

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = tx
            .query_row(
                "SELECT final_filename FROM filename_reservations WHERE instance_id=?1",
                params![instance_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            tx.commit()?;
            return Ok(existing);
        }

        let (instance_sha, instance_path): (String, String) = tx.query_row(
            "SELECT sha256, original_path FROM file_instances WHERE instance_id=?1",
            params![instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        for n in 1..=500u32 {
            let stem = if n == 1 {
                base.to_string()
            } else {
                format!("{base} ({n})")
            };
            let candidate = if ext.is_empty() {
                stem
            } else {
                format!("{stem}.{ext}")
            };

            let reserved: bool = tx.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM filename_reservations
                    WHERE final_filename = ?1 COLLATE NOCASE
                 )",
                params![candidate],
                |row| row.get(0),
            )?;
            if reserved {
                continue;
            }

            let legacy_owner: Option<(String, String)> = tx
                .query_row(
                    "SELECT sha256, original_path FROM jobs
                     WHERE final_filename IS NOT NULL
                       AND final_filename = ?1 COLLATE NOCASE
                     LIMIT 1",
                    params![candidate],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;

            let legacy_conflict = legacy_owner.is_some_and(|(sha, path)| {
                sha != instance_sha
                    || crate::identity::normalize_relpath(&path)
                        != crate::identity::normalize_relpath(&instance_path)
            });
            if legacy_conflict {
                continue;
            }

            tx.execute(
                "INSERT INTO filename_reservations (final_filename, instance_id)
                 VALUES (?1, ?2)",
                params![candidate, instance_id],
            )?;
            tx.execute(
                "UPDATE file_instances
                 SET final_filename=?2,
                     updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE instance_id=?1",
                params![instance_id, candidate],
            )?;
            tx.commit()?;
            return Ok(candidate);
        }

        anyhow::bail!("collision resolution runaway for '{base}'")
    }

    /// Insert a content job if unseen. Returns the existing content job for a
    /// duplicate or a resume.
    pub fn ingest(
        &self,
        sha256: &str,
        original_path: &str,
        original_name: &str,
        ext: &str,
    ) -> anyhow::Result<Option<Job>> {
        let conn = self.conn.lock().unwrap();
        if let Some(job) = Self::get_inner(&conn, sha256)? {
            return Ok(Some(job));
        }
        conn.execute(
            "INSERT INTO jobs (sha256, original_path, original_name, ext, state)
             VALUES (?1, ?2, ?3, ?4, 'ingested')",
            params![sha256, original_path, original_name, ext],
        )?;
        Ok(None)
    }

    pub fn get(&self, sha256: &str) -> anyhow::Result<Option<Job>> {
        let conn = self.conn.lock().unwrap();
        Self::get_inner(&conn, sha256)
    }

    fn get_inner(conn: &Connection, sha256: &str) -> anyhow::Result<Option<Job>> {
        let job = conn
            .query_row(
                "SELECT * FROM jobs WHERE sha256 = ?1",
                params![sha256],
                Self::row_to_job,
            )
            .optional()?;
        Ok(job)
    }

    fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
        Ok(Job {
            sha256: row.get("sha256")?,
            original_path: row.get("original_path")?,
            original_name: row.get("original_name")?,
            ext: row.get("ext")?,
            detected_type: row.get("detected_type")?,
            route: row.get("route")?,
            state: JobState::parse(&row.get::<_, String>("state")?).unwrap_or(JobState::Flagged),
            attempts: row.get::<_, i64>("attempts")? as u8,
            flag_reason: row.get("flag_reason")?,
            proposed_date: row.get("proposed_date")?,
            date_source: row.get("date_source")?,
            proposed_subject: row.get("proposed_subject")?,
            description: row.get("description")?,
            final_filename: row.get("final_filename")?,
            doc_type: row.get("doc_type")?,
            language: row.get("language")?,
            duplicate_of: row.get("duplicate_of")?,
            soft_flags: row.get("soft_flags")?,
            model_versions: row.get("model_versions")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    pub fn set_state(&self, sha256: &str, state: JobState) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET state=?2, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE sha256=?1",
            params![sha256, state.as_str()],
        )?;
        Ok(())
    }

    pub fn update_fields(
        &self,
        sha256: &str,
        kv: &[(&str, Option<String>)],
    ) -> anyhow::Result<()> {
        // Column names come from a fixed allowlist at call sites; assert anyway.
        const ALLOWED: &[&str] = &[
            "detected_type",
            "route",
            "flag_reason",
            "proposed_date",
            "date_source",
            "proposed_subject",
            "description",
            "final_filename",
            "doc_type",
            "language",
            "duplicate_of",
            "soft_flags",
            "model_versions",
        ];
        let conn = self.conn.lock().unwrap();
        for (col, val) in kv {
            assert!(ALLOWED.contains(col), "disallowed ledger column: {col}");
            let sql = format!(
                "UPDATE jobs SET {col}=?2, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE sha256=?1"
            );
            conn.execute(&sql, params![sha256, val])?;
        }
        Ok(())
    }

    pub fn bump_attempts(&self, sha256: &str) -> anyhow::Result<u8> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET attempts = attempts + 1 WHERE sha256=?1",
            params![sha256],
        )?;
        let n: i64 = conn.query_row(
            "SELECT attempts FROM jobs WHERE sha256=?1",
            params![sha256],
            |row| row.get(0),
        )?;
        Ok(n as u8)
    }

    pub fn reset_attempts(&self, sha256: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET attempts=0 WHERE sha256=?1",
            params![sha256],
        )?;
        Ok(())
    }

    pub fn log_event(&self, sha256: &str, stage: &str, detail: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO events (sha256, stage, detail) VALUES (?1, ?2, ?3)",
            params![sha256, stage, detail],
        )?;
        Ok(())
    }

    /// Legacy content-level collision helper. New pipeline code should reserve
    /// names through `reserve_filename`, which is instance-aware and atomic.
    pub fn dedupe_name(&self, base: &str, ext: &str) -> anyhow::Result<String> {
        let conn = self.conn.lock().unwrap();
        let mut candidate = base.to_string();
        let mut n = 1u32;
        loop {
            let full = format!("{candidate}.{ext}");
            let exists: bool = conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM jobs WHERE final_filename = ?1 COLLATE NOCASE
                    UNION ALL
                    SELECT 1 FROM filename_reservations WHERE final_filename = ?1 COLLATE NOCASE
                 )",
                params![full],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(candidate);
            }
            n += 1;
            candidate = format!("{base} ({n})");
            if n > 500 {
                anyhow::bail!("collision resolution runaway for '{base}'");
            }
        }
    }

    pub fn list_jobs(&self, limit: usize) -> anyhow::Result<Vec<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM jobs ORDER BY updated_at DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], Self::row_to_job)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn list_by_state(&self, state: JobState) -> anyhow::Result<Vec<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM jobs WHERE state=?1 ORDER BY updated_at DESC")?;
        let rows = stmt.query_map(params![state.as_str()], Self::row_to_job)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// Content jobs that were mid-flight when the app died.
    pub fn resumable(&self) -> anyhow::Result<Vec<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM jobs WHERE state NOT IN ('emitted','flagged') ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], Self::row_to_job)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn resumable_instances(&self) -> anyhow::Result<Vec<FileInstance>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM file_instances
             WHERE state NOT IN ('emitted','flagged')
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], Self::row_to_instance)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn stats(&self) -> anyhow::Result<serde_json::Value> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT state, COUNT(*) FROM jobs GROUP BY state")?;
        let mut map = serde_json::Map::new();
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows.flatten() {
            map.insert(row.0, serde_json::json!(row.1));
        }
        Ok(serde_json::Value::Object(map))
    }
}
