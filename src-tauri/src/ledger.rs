//! The job ledger. SQLite, content-hash keyed. Every file's journey is
//! recorded as an explicit state machine so a crash mid-batch resumes exactly
//! where it died, no file is processed twice, and every emitted name is
//! reproducible (model versions are stored per job).

use rusqlite::{params, Connection, OptionalExtension};
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
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Insert if unseen. Returns:
    ///   Ok(None)              -> brand new job created
    ///   Ok(Some(existing))    -> hash already known (duplicate content or resume)
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
            .query_row("SELECT * FROM jobs WHERE sha256 = ?1", params![sha256], Self::row_to_job)
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

    pub fn update_fields(&self, sha256: &str, kv: &[(&str, Option<String>)]) -> anyhow::Result<()> {
        // Column names come from a fixed allowlist at call sites; enforce it
        // here too, but BEFORE taking the lock and as a returned error, not a
        // panic — panicking while holding the connection lock would poison it
        // and take down every subsequent ledger access.
        const ALLOWED: &[&str] = &[
            "detected_type", "route", "flag_reason", "proposed_date", "date_source",
            "proposed_subject", "description", "final_filename", "doc_type",
            "language", "duplicate_of", "soft_flags", "model_versions",
        ];
        for (col, _) in kv {
            if !ALLOWED.contains(col) {
                anyhow::bail!("disallowed ledger column: {col}");
            }
        }
        let conn = self.conn.lock().unwrap();
        for (col, val) in kv {
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
        let n: i64 =
            conn.query_row("SELECT attempts FROM jobs WHERE sha256=?1", params![sha256], |r| r.get(0))?;
        Ok(n as u8)
    }

    pub fn reset_attempts(&self, sha256: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE jobs SET attempts=0 WHERE sha256=?1", params![sha256])?;
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

    /// Filename collision resolution against everything the ledger has emitted.
    /// Returns the base name (no extension) with " (n)" appended as needed.
    /// `self_key` is the sha256 (or duplicate key) of the job being named; its
    /// own prior `final_filename` is excluded so a resumed job doesn't collide
    /// with itself and drift to " (2)".
    pub fn dedupe_name(&self, base: &str, ext: &str, self_key: &str) -> anyhow::Result<String> {
        let conn = self.conn.lock().unwrap();
        let mut candidate = base.to_string();
        let mut n = 1u32;
        loop {
            let full = format!("{candidate}.{ext}");
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE final_filename = ?1 AND sha256 <> ?2)",
                params![full, self_key],
                |r| r.get(0),
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
        let mut stmt =
            conn.prepare("SELECT * FROM jobs ORDER BY updated_at DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], Self::row_to_job)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn list_by_state(&self, state: JobState) -> anyhow::Result<Vec<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM jobs WHERE state=?1 ORDER BY updated_at DESC")?;
        let rows = stmt.query_map(params![state.as_str()], Self::row_to_job)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// Jobs that were mid-flight when the app died: anything not terminal.
    pub fn resumable(&self) -> anyhow::Result<Vec<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM jobs WHERE state NOT IN ('emitted','flagged') ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], Self::row_to_job)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn stats(&self) -> anyhow::Result<serde_json::Value> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT state, COUNT(*) FROM jobs GROUP BY state")?;
        let mut map = serde_json::Map::new();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows.flatten() {
            map.insert(row.0, serde_json::json!(row.1));
        }
        Ok(serde_json::Value::Object(map))
    }
}
