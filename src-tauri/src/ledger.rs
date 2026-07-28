//! The job ledger. SQLite, content-hash keyed. Every file's journey is
//! recorded as an explicit state machine so a crash mid-batch resumes exactly
//! where it died, no file is processed twice, and every emitted name is
//! reproducible (model versions are stored per job).
//!
//! Two invariants carry that promise, and both are enforced here rather than
//! in the orchestrator, because the orchestrator runs many workers at once:
//!   * `try_claim` is the single atomic "I own this job" gate — the startup
//!     sweep and a watcher event racing on the same file resolve to one winner.
//!   * `set_state` is a guarded compare-and-swap, so a losing or stale worker
//!     can never walk a terminal job backwards into a state a human would then
//!     be asked to correct.

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

use crate::dbkey;

/// The first 16 bytes of every unencrypted SQLite file. SQLCipher
/// randomizes these (they're the first bytes of ciphertext instead), so
/// their presence is both how we detect a pre-encryption plaintext db to
/// migrate away from, and — inverted — the proof-of-encryption check in
/// this module's tests.
const SQLITE_PLAINTEXT_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// Read just the first 16 bytes of `path` and compare against the
/// unencrypted-SQLite magic header. `false` for any read error (missing
/// file, too short, permissions) as well as a genuine mismatch — callers
/// only use this for a log message, never as a correctness gate.
fn looks_like_plaintext_sqlite(path: &Path) -> bool {
    let mut buf = [0u8; 16];
    match std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => &buf == SQLITE_PLAINTEXT_HEADER,
        Err(_) => false,
    }
}

/// The timestamp format every column in this ledger uses. Identical to
/// SQLite's `strftime('%Y-%m-%dT%H:%M:%fZ','now')` so values written from Rust
/// and from SQL sort together — the claim staleness check and the events TTL
/// are plain lexicographic string comparisons and would silently mis-order
/// otherwise.
fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn iso_secs_ago(secs: u64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(secs as i64))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

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
    /// Operator decided this file needs no name at all (junk, a stray sync
    /// artifact, a duplicate they don't want indexed). Terminal, and distinct
    /// from Emitted so throughput numbers don't count it as work delivered.
    Dismissed,
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
            JobState::Dismissed => "dismissed",
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
            "dismissed" => JobState::Dismissed,
            _ => return None,
        })
    }

    /// Rung on the documented ladder, or `None` for the off-ladder outcomes
    /// (flagged/dismissed). Used to tell "the job advanced" from "the job
    /// replayed a stage it had already reached".
    fn ladder_rank(self) -> Option<u8> {
        Some(match self {
            JobState::Ingested => 0,
            JobState::Converted => 1,
            JobState::Filtered => 2,
            JobState::Named => 3,
            JobState::Validated => 4,
            JobState::Emitted => 5,
            JobState::Flagged | JobState::Dismissed => return None,
        })
    }

    /// A job whose outcome is final. Nothing may move it, and its cached
    /// document text is safe to sweep.
    pub fn is_resolved(self) -> bool {
        matches!(self, JobState::Emitted | JobState::Dismissed)
    }
}

/// Which state transitions the ledger will actually perform.
///
/// The ladder is documented as forward-only, but a job resumed after a crash
/// re-runs from ingest and legitimately re-announces stages it already passed
/// (`named -> converted` on the replay). So re-entry among the in-flight
/// states is allowed; what is refused is resurrecting a finished job. That is
/// the containment this module is for: a losing worker that finishes late must
/// not be able to retro-flag a document Flow 2 has already archived.
fn transition_allowed(from: JobState, to: JobState) -> bool {
    use JobState::*;
    match (from, to) {
        // Terminal outcomes are frozen.
        (Emitted, _) | (Dismissed, _) => false,
        // A flagged file leaves quarantine only through the audited resubmit
        // path (-> Emitted) or an explicit operator dismissal.
        (Flagged, Emitted) | (Flagged, Dismissed) => true,
        (Flagged, _) => false,
        // Anything still in flight may be flagged or dismissed, or replay an
        // earlier rung of the ladder.
        _ => true,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub sha256: String,
    pub original_path: String,
    pub original_name: String,
    /// Processing-relative path, verbatim apart from separator unification —
    /// exactly what the manifest carries. Identity is relative everywhere else,
    /// so storing it at ingest means a later change to the configured
    /// Processing folder cannot silently re-key or reclassify old jobs.
    /// Normalization (case, separators) belongs at the comparison, not here.
    pub original_relpath: Option<String>,
    pub ext: String,
    pub detected_type: String,
    pub route: String,
    pub state: JobState,
    /// Consecutive claims taken while the job sat at `last_stage`. Reset the
    /// moment it advances, so a healthy file that merely got re-enqueued is
    /// never mistaken for a poison pill.
    pub attempts: i64,
    pub last_stage: Option<String>,
    /// The stage a worker is inside RIGHT NOW ("convert", "filter", "name"…),
    /// stamped when the stage begins. Deliberately not `last_stage`: that
    /// column holds ladder states and drives both the crash-loop counter
    /// (`bump_stage_attempts`) and the "did it advance?" test in `set_state`,
    /// so writing an off-ladder token into it would reset the poison-pill
    /// count on every claim. Without this column the wall-clock timeout could
    /// only name the last stage that *finished*, so a file that hung in OCR
    /// was reported as "at stage ingested".
    pub active_stage: Option<String>,
    pub stage_started_at: Option<String>,
    pub claimed_at: Option<String>,
    pub flag_reason: Option<String>,
    /// Where `flag()` actually put the file. The read path must never
    /// reconstruct this from the leaf name — two flagged `scan.pdf` files land
    /// under different quarantine names on purpose.
    pub quarantine_path: Option<String>,
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

/// One row of the forensic trail: per-attempt OCR confidence, checker
/// rejection codes, span-mismatch re-prompts, human corrections.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: i64,
    pub sha256: String,
    pub at: String,
    pub stage: String,
    pub detail: String,
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
CREATE INDEX IF NOT EXISTS idx_events_at ON events(at);
"#;

/// Columns added after the first schema shipped. `CREATE TABLE IF NOT EXISTS`
/// is a no-op against an existing ledger, so a pilot db from an earlier build
/// would otherwise be missing every column the claim, crash-loop and
/// quarantine bookkeeping below depend on.
const ADDED_COLUMNS: &[(&str, &str)] = &[
    ("original_relpath", "TEXT"),
    ("claimed_at", "TEXT"),
    ("last_stage", "TEXT"),
    ("active_stage", "TEXT"),
    ("stage_started_at", "TEXT"),
    ("quarantine_path", "TEXT"),
];

impl Ledger {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Key blob lives next to the db (ledger.db -> ledger.key).
        let key_path = path.with_extension("key");

        if !key_path.exists() && path.exists() {
            Self::migrate_unkeyed_db(path, &key_path)?;
        }

        let key = dbkey::resolve_key(&key_path)?;

        let conn = Connection::open(path)?;
        // SQLCipher requires the key to be set before any other statement
        // on the connection — a PRAGMA journal_mode or a schema read first
        // would silently operate on/create a plaintext db instead of an
        // encrypted one. The `x'<hex>'` raw-key form (vs. a passphrase
        // string) skips SQLCipher's PBKDF2 key derivation, which exists to
        // stretch low-entropy human passphrases; our key is already a
        // uniformly random 256-bit CSPRNG output, so deriving further from
        // it would add cost with no security benefit.
        //
        // The statement is built by `SecretKey::pragma_statement` rather than
        // `format!` here: a `format!` allocates a second, unscrubbed copy of
        // the key that stays legible in freed heap — and therefore in any
        // crash dump, hibernation file or page file — for the life of the
        // process. `ZeroizingString` overwrites its buffer on drop.
        conn.execute_batch(&key.pragma_statement())?;
        // busy_timeout so a writer that briefly loses the file lock (WAL
        // checkpoint, an OS-level scanner) retries instead of surfacing
        // SQLITE_BUSY as a lost job update.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Self::ensure_columns(&conn)?;
        Self::ensure_unique_final_filename(&conn);
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn ensure_columns(conn: &Connection) -> anyhow::Result<()> {
        let existing: HashSet<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(jobs)")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
            rows.flatten().collect()
        };
        for (name, ty) in ADDED_COLUMNS {
            if !existing.contains(*name) {
                conn.execute_batch(&format!("ALTER TABLE jobs ADD COLUMN {name} {ty};"))?;
            }
        }
        Ok(())
    }

    /// Backstop for `reserve_name`: the reservation is serialized in-process,
    /// but the index is what turns a lost race into a retryable constraint
    /// violation instead of two jobs silently booking one Archive filename.
    ///
    /// Best-effort on purpose — a pilot ledger written before the index existed
    /// may already contain a duplicate, and refusing to open at all would take
    /// the appliance down over a name collision a human can still resolve.
    fn ensure_unique_final_filename(conn: &Connection) {
        if let Err(e) = conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_final_unique
             ON jobs(final_filename) WHERE final_filename IS NOT NULL;",
        ) {
            log::warn!(
                "could not enforce unique final_filename (existing duplicates?): {e}; \
                 name reservation stays serialized in-process"
            );
        }
    }

    /// Handle the one-time transition to encryption: a db file already
    /// exists at `path` but no key file exists for it yet. That's either a
    /// pre-encryption plaintext dev db, or (far less likely) a previously
    /// encrypted db whose key file was separately lost — either way we
    /// have no key that opens it. BackLog is a pilot with no production
    /// data riding on this ledger, so the robust, simple choice is: never
    /// destroy the old file, move it aside as a `.plaintext.bak`, and let
    /// `open` create a fresh encrypted db in its place. (The alternative,
    /// an in-place `sqlcipher_export` migration, only applies to the
    /// plaintext case anyway and adds an ATTACH/DETACH failure mode for a
    /// db that — by definition, since we're in a pilot — is safe to
    /// recreate.)
    fn migrate_unkeyed_db(path: &Path, key_path: &Path) -> anyhow::Result<()> {
        let is_plaintext = looks_like_plaintext_sqlite(path);
        let file_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let backup_path = path.with_file_name(format!("{file_name}.plaintext.bak"));

        log::warn!(
            "no ledger key found at {} but {} already exists (plaintext_sqlite_header={is_plaintext}); \
             moving it to {} and starting a fresh SQLCipher-encrypted ledger",
            key_path.display(),
            path.display(),
            backup_path.display(),
        );

        // Best-effort: fold any WAL content into the main file before we
        // move it, so nothing PII-bearing is left behind in a stray -wal
        // sidecar. Ignored if this isn't actually a plain SQLite db.
        if let Ok(old_conn) = Connection::open(path) {
            let _ = old_conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }

        std::fs::rename(path, &backup_path)?;
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(path.with_file_name(format!("{file_name}{suffix}")));
        }
        Ok(())
    }

    /// Insert if unseen. Returns:
    ///   Ok(None)              -> brand new job created
    ///   Ok(Some(existing))    -> hash already known (duplicate content or resume)
    ///
    /// Row creation is atomic, but it is NOT a claim: two workers that both
    /// see `Ok(Some(existing))` for a mid-flight job must still fight over
    /// `try_claim` before either of them does any work.
    pub fn ingest(
        &self,
        sha256: &str,
        original_path: &str,
        original_name: &str,
        original_relpath: &str,
        ext: &str,
    ) -> anyhow::Result<Option<Job>> {
        let conn = self.conn.lock().unwrap();
        if let Some(job) = Self::get_inner(&conn, sha256)? {
            return Ok(Some(job));
        }
        conn.execute(
            "INSERT INTO jobs (sha256, original_path, original_name, original_relpath, ext, state)
             VALUES (?1, ?2, ?3, ?4, ?5, 'ingested')",
            params![sha256, original_path, original_name, original_relpath, ext],
        )?;
        Ok(None)
    }

    pub fn get(&self, sha256: &str) -> anyhow::Result<Option<Job>> {
        let conn = self.conn.lock().unwrap();
        Self::get_inner(&conn, sha256)
    }

    /// Just the state, for hot paths (the startup cache sweep walks every
    /// cached file and only needs to know whether the job is resolved).
    pub fn job_state(&self, sha256: &str) -> anyhow::Result<Option<JobState>> {
        let conn = self.conn.lock().unwrap();
        let state: Option<String> = conn
            .query_row(
                "SELECT state FROM jobs WHERE sha256=?1",
                params![sha256],
                |r| r.get(0),
            )
            .optional()?;
        Ok(state.map(|s| JobState::parse(&s).unwrap_or(JobState::Flagged)))
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
            original_relpath: row.get("original_relpath")?,
            ext: row.get("ext")?,
            detected_type: row.get("detected_type")?,
            route: row.get("route")?,
            state: JobState::parse(&row.get::<_, String>("state")?).unwrap_or(JobState::Flagged),
            attempts: row.get("attempts")?,
            last_stage: row.get("last_stage")?,
            active_stage: row.get("active_stage")?,
            stage_started_at: row.get("stage_started_at")?,
            claimed_at: row.get("claimed_at")?,
            flag_reason: row.get("flag_reason")?,
            quarantine_path: row.get("quarantine_path")?,
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

    /// Atomically take ownership of a job. Exactly one caller wins; everyone
    /// else gets `false` and must abandon the file quietly.
    ///
    /// A claim older than `stale_after_secs` is reclaimable: the previous
    /// owner is a process that died holding it, and nothing else would ever
    /// release it.
    pub fn try_claim(&self, sha256: &str, stale_after_secs: u64) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE jobs SET claimed_at=?2
             WHERE sha256=?1 AND (claimed_at IS NULL OR claimed_at < ?3)",
            params![sha256, now_iso(), iso_secs_ago(stale_after_secs)],
        )?;
        Ok(changed == 1)
    }

    pub fn release_claim(&self, sha256: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET claimed_at=NULL WHERE sha256=?1",
            params![sha256],
        )?;
        Ok(())
    }

    /// Record that a claim was taken while the job sat at `stage`, and return
    /// how many consecutive claims it has now taken there.
    ///
    /// Counting claims-at-a-stage rather than enqueues is what separates a
    /// genuine poison pill (keeps dying at the same rung) from a healthy file
    /// that was merely re-enqueued by a duplicate watcher event.
    pub fn bump_stage_attempts(&self, sha256: &str, stage: &str) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let now = now_iso();
        conn.execute(
            // `active_stage` belongs to the run that just ended; a fresh claim
            // must not inherit it, or a timeout before the first stage begins
            // would name a stage from a previous process.
            "UPDATE jobs SET
                 attempts = CASE WHEN last_stage IS ?2 THEN attempts + 1 ELSE 1 END,
                 last_stage = ?2,
                 active_stage = NULL,
                 stage_started_at = ?3,
                 updated_at = ?3
             WHERE sha256=?1",
            params![sha256, stage, now],
        )?;
        let attempts: Option<i64> = conn
            .query_row(
                "SELECT attempts FROM jobs WHERE sha256=?1",
                params![sha256],
                |r| r.get(0),
            )
            .optional()?;
        Ok(attempts.unwrap_or(0))
    }

    /// Record that a blocking stage has just STARTED, so anything looking at
    /// the row knows where the file actually is rather than where it last
    /// succeeded. `last_stage` is written only after a stage completes, which
    /// left the wall-clock timeout naming the previous rung: a file wedged in
    /// OCR was reported "at stage ingested" and one wedged in the SLM "at stage
    /// converted" — off by one, on the one surface a non-technical operator
    /// reads.
    pub fn mark_stage(&self, sha256: &str, stage: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET active_stage=?2, stage_started_at=?3 WHERE sha256=?1",
            params![sha256, stage, now_iso()],
        )?;
        Ok(())
    }

    /// Guarded compare-and-swap on the state machine. `Ok(false)` means the
    /// transition was refused — the job is gone, already terminal, or another
    /// worker moved it — and the caller must abandon it, not retry.
    ///
    /// A successful move also stamps `last_stage`/`stage_started_at` (so the
    /// wall-clock timeout can say where a file died) and clears the crash-loop
    /// counter whenever the job genuinely advanced a rung.
    pub fn set_state(&self, sha256: &str, state: JobState) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let row: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT state, last_stage FROM jobs WHERE sha256=?1",
                params![sha256],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((current_raw, last_stage)) = row else {
            return Ok(false);
        };
        let current = JobState::parse(&current_raw).unwrap_or(JobState::Flagged);
        if !transition_allowed(current, state) {
            return Ok(false);
        }
        let advanced = match (
            state.ladder_rank(),
            last_stage
                .as_deref()
                .and_then(JobState::parse)
                .and_then(JobState::ladder_rank),
        ) {
            (Some(new_rank), Some(old_rank)) => new_rank > old_rank,
            (Some(_), None) => true,
            (None, _) => false, // flagged/dismissed is an outcome, not progress
        };
        let now = now_iso();
        // The predicate re-asserts the state observed above; the connection
        // mutex is held across both statements, so this is a genuine CAS.
        let changed = conn.execute(
            "UPDATE jobs SET state=?2, last_stage=?2, stage_started_at=?3,
                 attempts = CASE WHEN ?4 THEN 0 ELSE attempts END, updated_at=?3
             WHERE sha256=?1 AND state=?5",
            params![sha256, state.as_str(), now, advanced, current_raw],
        )?;
        Ok(changed == 1)
    }

    /// Whole-slice write in one statement (and therefore one implicit
    /// transaction). The previous column-per-transaction form cost ~20 commits
    /// per file, each taken while holding the global connection mutex — the
    /// ledger's single largest contribution to per-file wall clock and to WAL
    /// growth on a multi-thousand-file backfill.
    pub fn update_fields(&self, sha256: &str, kv: &[(&str, Option<String>)]) -> anyhow::Result<()> {
        // Column names come from a fixed allowlist at call sites; enforce it
        // here too, but BEFORE taking the lock and as a returned error, not a
        // panic — panicking while holding the connection lock would poison it
        // and take down every subsequent ledger access.
        const ALLOWED: &[&str] = &[
            "detected_type",
            "route",
            "flag_reason",
            "quarantine_path",
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
        if kv.is_empty() {
            return Ok(());
        }
        let mut seen = HashSet::new();
        for (col, _) in kv {
            if !ALLOWED.contains(col) {
                anyhow::bail!("disallowed ledger column: {col}");
            }
            if !seen.insert(*col) {
                anyhow::bail!("column {col} assigned twice in one update");
            }
        }
        // Positional `?` parameters bind in iteration order: sha, values…, now.
        let assignments = kv
            .iter()
            .map(|(c, _)| format!("{c}=?"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("UPDATE jobs SET {assignments}, updated_at=? WHERE sha256=?");
        let mut values: Vec<Option<String>> = Vec::with_capacity(kv.len() + 2);
        for (_, v) in kv {
            values.push(v.clone());
        }
        values.push(Some(now_iso()));
        values.push(Some(sha256.to_string()));

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(&sql)?;
        stmt.execute(params_from_iter(values))?;
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

    /// Forensic trail for one job, newest first. The events table is written
    /// at a dozen sites with the only record of *why* a file was named the way
    /// it was; without a read path that diagnosis is thrown away.
    pub fn events_for(&self, sha256: &str, limit: usize) -> anyhow::Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, sha256, at, stage, detail FROM events
             WHERE sha256=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![sha256, limit as i64], |r| {
            Ok(Event {
                id: r.get(0)?,
                sha256: r.get(1)?,
                at: r.get(2)?,
                stage: r.get(3)?,
                detail: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// Drop forensic events older than `ttl_days`. The events table is the one
    /// store with no retention policy and the one accumulating the most
    /// sensitive derived text, which contradicts the product's stated posture
    /// that document text is purged on emit.
    pub fn sweep_events(&self, ttl_days: u64) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let removed = conn.execute(
            "DELETE FROM events WHERE at < ?1",
            params![iso_secs_ago(ttl_days.saturating_mul(86_400))],
        )?;
        Ok(removed)
    }

    /// Atomically resolve and RESERVE a final filename, returning it.
    ///
    /// The previous read-only `dedupe_name` only SELECTed and then dropped the
    /// lock; the write happened separately in the orchestrator, so two jobs
    /// naming themselves at once both observed "free" and both committed the
    /// same name. Flow 2 keys its Archive copy on that name, so the collision
    /// surfaced as a Power Automate failure rather than anything a human could
    /// see or fix. Holding the connection mutex across BEGIN IMMEDIATE closes
    /// the window; the partial unique index is the backstop.
    ///
    /// `self_key` is the sha256 (or duplicate key) of the job being named; its
    /// own prior `final_filename` is excluded so a resumed job doesn't collide
    /// with itself and drift to " (2)".
    pub fn reserve_name(&self, base: &str, ext: &str, self_key: &str) -> anyhow::Result<String> {
        let mut guard = self.conn.lock().unwrap();
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut candidate = base.to_string();
        let mut n = 1u32;
        loop {
            let full = format!("{candidate}.{ext}");
            let taken: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE final_filename = ?1 AND sha256 <> ?2)",
                params![full, self_key],
                |r| r.get(0),
            )?;
            if !taken {
                match tx.execute(
                    "UPDATE jobs SET final_filename=?2, updated_at=?3 WHERE sha256=?1",
                    params![self_key, full, now_iso()],
                ) {
                    Ok(1) => {
                        tx.commit()?;
                        return Ok(full);
                    }
                    Ok(_) => anyhow::bail!("cannot reserve a name for unknown job {self_key}"),
                    // Lost to a writer outside this connection: the index did
                    // its job, so try the next suffix instead of double-booking.
                    Err(rusqlite::Error::SqliteFailure(e, _))
                        if e.code == rusqlite::ErrorCode::ConstraintViolation => {}
                    Err(e) => return Err(e.into()),
                }
            }
            n += 1;
            candidate = format!("{base} ({n})");
            if n > 500 {
                anyhow::bail!("collision resolution runaway for '{base}'");
            }
        }
    }

    /// Clear every derived field and put the job back at the head of the
    /// ladder so the operator can retry a file after fixing what broke it.
    /// One statement, so a half-reset row can never be observed.
    pub fn reset_for_reprocess(&self, sha256: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE jobs SET state='ingested', attempts=0, claimed_at=NULL, last_stage=NULL,
                 active_stage=NULL,
                 stage_started_at=NULL, flag_reason=NULL, quarantine_path=NULL, soft_flags=NULL,
                 final_filename=NULL, proposed_date=NULL, date_source=NULL,
                 proposed_subject=NULL, description=NULL, updated_at=?2
             WHERE sha256=?1",
            params![sha256, now_iso()],
        )?;
        Ok(changed == 1)
    }

    /// Shared WHERE fragment + bound values for the search/count pair, so a
    /// count can never drift from the page it describes.
    fn job_filter(
        query: Option<&str>,
        state: Option<JobState>,
    ) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        let mut sql = String::new();
        let mut values = Vec::new();
        if let Some(q) = query.map(str::trim).filter(|q| !q.is_empty()) {
            // LIKE wildcards typed by a human are literals, not operators.
            let escaped = q
                .replace('\\', r"\\")
                .replace('%', r"\%")
                .replace('_', r"\_");
            let pattern = format!("%{escaped}%");
            sql.push_str(
                " AND (original_name LIKE ? ESCAPE '\\' OR final_filename LIKE ? ESCAPE '\\')",
            );
            values.push(Value::Text(pattern.clone()));
            values.push(Value::Text(pattern));
        }
        if let Some(state) = state {
            sql.push_str(" AND state = ?");
            values.push(Value::Text(state.as_str().to_string()));
        }
        (sql, values)
    }

    /// Paged, filtered job listing — the backing query for a review surface
    /// that has to work against a ledger holding a whole backfill.
    pub fn search_jobs(
        &self,
        query: Option<&str>,
        state: Option<JobState>,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<Job>> {
        use rusqlite::types::Value;
        let (filter, mut values) = Self::job_filter(query, state);
        let sql = format!(
            "SELECT * FROM jobs WHERE 1=1{filter} ORDER BY updated_at DESC LIMIT ? OFFSET ?"
        );
        values.push(Value::Integer(limit as i64));
        values.push(Value::Integer(offset as i64));
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), Self::row_to_job)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn count_jobs(&self, query: Option<&str>, state: Option<JobState>) -> anyhow::Result<i64> {
        let (filter, values) = Self::job_filter(query, state);
        let sql = format!("SELECT COUNT(*) FROM jobs WHERE 1=1{filter}");
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(&sql)?;
        let n = stmt.query_row(params_from_iter(values), |r| r.get(0))?;
        Ok(n)
    }

    /// Paged flagged/NeedsReview listing. The unbounded form this replaces
    /// shipped every flagged row in a whole backfill across the IPC boundary
    /// to render one screen.
    pub fn list_by_state_paged(
        &self,
        state: JobState,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<Job>> {
        self.search_jobs(None, Some(state), limit, offset)
    }

    /// Jobs that reached a terminal outcome inside the last `window_mins` —
    /// the honest "files per hour" number for a backfill that has to be
    /// estimated before it is started.
    pub fn throughput(&self, window_mins: u64) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM jobs
             WHERE state IN ('emitted','flagged','dismissed') AND updated_at >= ?1",
            params![iso_secs_ago(window_mins.saturating_mul(60))],
            |r| r.get(0),
        )?;
        Ok(n)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const MARKER: &str = "SECRET_SUBJECT_MARKER";

    fn ledger() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.db")).unwrap();
        (dir, ledger)
    }

    fn seed(ledger: &Ledger, sha: &str) {
        ledger
            .ingest(
                sha,
                &format!("C:/Processing/{sha}.pdf"),
                "doc.pdf",
                "doc.pdf",
                "pdf",
            )
            .unwrap();
    }

    /// This is the feature's actual proof: the ledger holds derived PII
    /// (subjects, descriptions, filenames, paths), so it must be
    /// unreadable outside the app. Opens a Ledger, writes a job carrying a
    /// recognizable "PII" marker, drops it (closing the connection), then
    /// inspects the *raw file bytes on disk* rather than going back
    /// through SQLite/rusqlite — a bug that left the db in plaintext would
    /// otherwise hide behind the very API that decrypts it.
    #[test]
    fn ledger_db_is_encrypted_at_rest_and_key_persists() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.db");

        {
            let ledger = Ledger::open(&db_path).unwrap();
            ledger
                .ingest(
                    "deadbeef",
                    "C:/Users/someone/Documents/secret.pdf",
                    "secret.pdf",
                    "secret.pdf",
                    "pdf",
                )
                .unwrap();
            ledger
                .update_fields(
                    "deadbeef",
                    &[("proposed_subject", Some(MARKER.to_string()))],
                )
                .unwrap();
        } // Ledger (and its Connection) dropped here — nothing left in memory or WAL.

        let raw = std::fs::read(&db_path).unwrap();

        // SQLCipher randomizes the header that every plaintext SQLite file
        // starts with, first-page salt included, so the header bytes
        // themselves are the encryption tell.
        assert!(
            !raw.starts_with(SQLITE_PLAINTEXT_HEADER),
            "ledger.db still starts with the plaintext SQLite header — it is NOT encrypted"
        );

        // The marker must not appear anywhere in the file, encoded any of
        // the ways SQLite might store a TEXT value (this is a raw byte
        // search, not a decode — it's deliberately looking for the exact
        // ASCII/UTF-8 the plaintext PII would have been stored as).
        let marker_bytes = MARKER.as_bytes();
        let found = raw.windows(marker_bytes.len()).any(|w| w == marker_bytes);
        assert!(
            !found,
            "plaintext PII marker found in raw ledger.db bytes — encryption is not effective"
        );

        // Key persistence: a second `open` against the same key file must
        // decrypt the *same* db and read the job back correctly.
        let ledger2 = Ledger::open(&db_path).unwrap();
        let job = ledger2
            .get("deadbeef")
            .unwrap()
            .expect("job should round-trip");
        assert_eq!(job.proposed_subject.as_deref(), Some(MARKER));
        assert_eq!(job.original_name, "secret.pdf");
    }

    /// A key file with no matching db (fresh install) must produce a
    /// working, still-encrypted ledger — not just "not crash".
    #[test]
    fn fresh_install_creates_encrypted_db_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.db");
        let key_path = dir.path().join("ledger.key");

        let ledger = Ledger::open(&db_path).unwrap();
        assert!(
            key_path.exists(),
            "key file should be created alongside a fresh db"
        );
        drop(ledger);

        let raw = std::fs::read(&db_path).unwrap();
        assert!(!raw.starts_with(SQLITE_PLAINTEXT_HEADER));
    }

    /// Pre-encryption dev dbs (plaintext, no key file yet) must be moved
    /// aside rather than silently reused or destroyed, and `open` must
    /// still succeed with a fresh encrypted db afterward.
    #[test]
    fn migrates_preexisting_plaintext_db_by_backing_it_up() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.db");
        let backup_path = dir.path().join("ledger.db.plaintext.bak");

        // Simulate a pre-encryption dev db: a plain SQLite file with no
        // PRAGMA key ever applied, and no ledger.key sitting next to it.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO jobs (sha256, original_path, original_name, ext, state) \
                 VALUES ('legacy', '/old/path.pdf', 'path.pdf', 'pdf', 'ingested')",
                [],
            )
            .unwrap();
        }
        let raw_before = std::fs::read(&db_path).unwrap();
        assert!(raw_before.starts_with(SQLITE_PLAINTEXT_HEADER));

        let ledger = Ledger::open(&db_path).unwrap();

        assert!(
            backup_path.exists(),
            "plaintext original should be preserved as a backup"
        );
        let backup_raw = std::fs::read(&backup_path).unwrap();
        assert!(
            backup_raw.starts_with(SQLITE_PLAINTEXT_HEADER),
            "backup should be the untouched plaintext file"
        );

        let fresh_raw = std::fs::read(&db_path).unwrap();
        assert!(
            !fresh_raw.starts_with(SQLITE_PLAINTEXT_HEADER),
            "the new ledger.db must be encrypted"
        );

        // Fresh db, not a decrypt of the old one — the legacy row is gone
        // from the live ledger (it's still recoverable from the backup).
        assert!(ledger.get("legacy").unwrap().is_none());
    }

    /// A ledger written by an earlier build has the jobs table already, so
    /// `CREATE TABLE IF NOT EXISTS` never runs; the claim/quarantine columns
    /// have to be added by migration or every write below fails at runtime.
    #[test]
    fn adds_missing_columns_to_an_existing_jobs_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.db");
        {
            let ledger = Ledger::open(&db_path).unwrap();
            seed(&ledger, "aa");
            // Simulate the pre-migration shape by dropping a new column back off.
            let conn = ledger.conn.lock().unwrap();
            conn.execute_batch("ALTER TABLE jobs DROP COLUMN quarantine_path;")
                .unwrap();
        }
        let ledger = Ledger::open(&db_path).unwrap();
        ledger
            .update_fields(
                "aa",
                &[("quarantine_path", Some("Q:/quarantine/x.pdf".into()))],
            )
            .unwrap();
        assert_eq!(
            ledger
                .get("aa")
                .unwrap()
                .unwrap()
                .quarantine_path
                .as_deref(),
            Some("Q:/quarantine/x.pdf")
        );
    }

    // ---- claims -----------------------------------------------------------

    /// The whole point of the claim: the startup sweep and a watcher event
    /// hitting the same file must resolve to exactly one worker.
    #[test]
    fn try_claim_admits_exactly_one_of_many_concurrent_callers() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "race");
        let ledger = Arc::new(ledger);

        let winners: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let ledger = ledger.clone();
                    scope.spawn(move || ledger.try_claim("race", 600).unwrap())
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().ok())
                .filter(|won| *won)
                .count()
        });

        assert_eq!(winners, 1, "exactly one caller may win the claim");
        // Releasing makes it claimable again, which is how a completed job is
        // re-processed after a human resubmits it.
        ledger.release_claim("race").unwrap();
        assert!(ledger.try_claim("race", 600).unwrap());
    }

    /// A process that died holding a claim must not wedge the file forever.
    #[test]
    fn a_stale_claim_is_reclaimable() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "stale");
        assert!(ledger.try_claim("stale", 600).unwrap());
        assert!(!ledger.try_claim("stale", 600).unwrap());
        // Zero staleness window: any claim already on the row counts as dead.
        // (Timestamps are millisecond-resolution, so give the clock a tick.)
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(ledger.try_claim("stale", 0).unwrap());
    }

    #[test]
    fn try_claim_on_an_unknown_job_is_false_not_an_error() {
        let (_dir, ledger) = ledger();
        assert!(!ledger.try_claim("nope", 600).unwrap());
    }

    // ---- state machine ----------------------------------------------------

    #[test]
    fn set_state_refuses_to_resurrect_a_terminal_job() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "term");
        assert!(ledger.set_state("term", JobState::Converted).unwrap());
        assert!(ledger.set_state("term", JobState::Emitted).unwrap());

        // The containment that P2's race needs: a losing worker finishing late
        // cannot drag an archived document back into the pipeline...
        assert!(!ledger.set_state("term", JobState::Converted).unwrap());
        // ...nor retro-flag it into NeedsReview.
        assert!(!ledger.set_state("term", JobState::Flagged).unwrap());
        assert_eq!(
            ledger.get("term").unwrap().unwrap().state,
            JobState::Emitted
        );
    }

    #[test]
    fn a_flagged_job_leaves_quarantine_only_through_resubmit_or_dismissal() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "flag");
        assert!(ledger.set_state("flag", JobState::Flagged).unwrap());
        assert!(!ledger.set_state("flag", JobState::Converted).unwrap());
        assert!(!ledger.set_state("flag", JobState::Flagged).unwrap());
        assert!(ledger.set_state("flag", JobState::Emitted).unwrap());

        seed(&ledger, "junk");
        assert!(ledger.set_state("junk", JobState::Flagged).unwrap());
        assert!(ledger.set_state("junk", JobState::Dismissed).unwrap());
        assert!(!ledger.set_state("junk", JobState::Emitted).unwrap());
    }

    /// A job resumed after a crash re-runs from ingest, so re-announcing an
    /// earlier rung is legitimate re-entry, not a regression to refuse.
    #[test]
    fn an_in_flight_job_may_replay_an_earlier_stage() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "resume");
        assert!(ledger.set_state("resume", JobState::Named).unwrap());
        assert!(ledger.set_state("resume", JobState::Converted).unwrap());
    }

    #[test]
    fn set_state_on_an_unknown_job_is_false_not_an_error() {
        let (_dir, ledger) = ledger();
        assert!(!ledger.set_state("ghost", JobState::Converted).unwrap());
    }

    // ---- crash-loop counter -----------------------------------------------

    /// Counting claims-at-a-stage, not enqueues: a file that keeps dying at
    /// the same rung accumulates attempts, and one that advances starts over.
    #[test]
    fn attempts_count_stalls_at_one_stage_and_reset_on_progress() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "loop");

        assert_eq!(ledger.bump_stage_attempts("loop", "ingested").unwrap(), 1);
        assert_eq!(ledger.bump_stage_attempts("loop", "ingested").unwrap(), 2);
        assert_eq!(ledger.bump_stage_attempts("loop", "ingested").unwrap(), 3);

        // Advancing a rung clears the counter, so a healthy file that was
        // merely re-enqueued never reaches the poison-pill ceiling.
        assert!(ledger.set_state("loop", JobState::Converted).unwrap());
        assert_eq!(ledger.get("loop").unwrap().unwrap().attempts, 0);
        assert_eq!(ledger.bump_stage_attempts("loop", "converted").unwrap(), 1);

        // Re-announcing the same rung is not progress.
        assert!(ledger.set_state("loop", JobState::Converted).unwrap());
        assert_eq!(ledger.get("loop").unwrap().unwrap().attempts, 1);
    }

    /// `mark_stage` has to name where the file IS without disturbing the
    /// ladder bookkeeping `last_stage` carries — writing an off-ladder token
    /// into that column would make every claim look like progress and reset the
    /// poison-pill count.
    #[test]
    fn mark_stage_names_the_running_stage_without_touching_the_crash_loop_count() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "where");

        assert_eq!(ledger.bump_stage_attempts("where", "ingested").unwrap(), 1);
        ledger.mark_stage("where", "convert").unwrap();
        let job = ledger.get("where").unwrap().unwrap();
        assert_eq!(job.active_stage.as_deref(), Some("convert"));
        assert!(job.stage_started_at.is_some());
        assert_eq!(
            job.last_stage.as_deref(),
            Some("ingested"),
            "the ladder column must still hold the last COMPLETED stage"
        );

        // A second claim at the same rung still counts as a stall...
        assert_eq!(ledger.bump_stage_attempts("where", "ingested").unwrap(), 2);
        // ...and does not inherit the dead run's stage.
        assert!(ledger.get("where").unwrap().unwrap().active_stage.is_none());
    }

    /// The old counter was cast to u8 and wrapped to 0 past 255, quietly
    /// resetting the crash-loop guard on the exact file it exists to catch.
    #[test]
    fn attempts_do_not_wrap_past_a_byte() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "big");
        {
            let conn = ledger.conn.lock().unwrap();
            conn.execute("UPDATE jobs SET attempts=300 WHERE sha256='big'", [])
                .unwrap();
        }
        assert_eq!(ledger.get("big").unwrap().unwrap().attempts, 300);
    }

    // ---- name reservation --------------------------------------------------

    /// Two files that produce the same "YYYY-MM-DD Subject" on the same day —
    /// a batch of scanned invoices from one vendor — must not both book it.
    #[test]
    fn reserve_name_hands_concurrent_callers_distinct_names() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "one");
        seed(&ledger, "two");
        let ledger = Arc::new(ledger);

        let mut names: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = ["one", "two"]
                .into_iter()
                .map(|key| {
                    let ledger = ledger.clone();
                    scope.spawn(move || {
                        ledger
                            .reserve_name("2024-03-05 Acme Invoice", "pdf", key)
                            .unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        names.sort();

        assert_eq!(
            names,
            vec![
                "2024-03-05 Acme Invoice (2).pdf",
                "2024-03-05 Acme Invoice.pdf"
            ]
        );
        // The reservation is the write: both names are durable in the ledger.
        let mut stored = ["one", "two"]
            .map(|k| ledger.get(k).unwrap().unwrap().final_filename.unwrap())
            .to_vec();
        stored.sort();
        assert_eq!(stored, names);
    }

    /// A resumed job re-reserving its own name must not drift to " (2)".
    #[test]
    fn reserve_name_is_idempotent_for_the_same_job() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "self");
        let first = ledger
            .reserve_name("2024-01-02 Lease Amendment", "pdf", "self")
            .unwrap();
        let again = ledger
            .reserve_name("2024-01-02 Lease Amendment", "pdf", "self")
            .unwrap();
        assert_eq!(first, again);
    }

    #[test]
    fn reserve_name_refuses_an_unknown_job() {
        let (_dir, ledger) = ledger();
        assert!(ledger
            .reserve_name("2024-01-02 Nothing Here", "pdf", "ghost")
            .is_err());
    }

    // ---- read surfaces -----------------------------------------------------

    #[test]
    fn events_round_trip_newest_first_and_sweep_by_age() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "ev");
        ledger
            .log_event("ev", "convert", "attempt 1 failed: SIDECAR")
            .unwrap();
        ledger
            .log_event("ev", "validate", "attempt 1 rejected: BAD_DATE")
            .unwrap();
        seed(&ledger, "other");
        ledger.log_event("other", "ingest", "ingested").unwrap();

        let events = ledger.events_for("ev", 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].stage, "validate");
        assert_eq!(events[1].detail, "attempt 1 failed: SIDECAR");
        assert_eq!(ledger.events_for("ev", 1).unwrap().len(), 1);

        // A TTL of 0 days puts the cutoff at "now", so everything already
        // written is past it.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(ledger.sweep_events(0).unwrap(), 3);
        assert!(ledger.events_for("ev", 10).unwrap().is_empty());
    }

    #[test]
    fn search_jobs_filters_paginates_and_counts_consistently() {
        let (_dir, ledger) = ledger();
        for (sha, name) in [
            ("a", "Acme invoice.pdf"),
            ("b", "Zenith lease.pdf"),
            ("c", "acme memo.docx"),
        ] {
            ledger
                .ingest(sha, &format!("C:/P/{name}"), name, name, "pdf")
                .unwrap();
        }
        ledger.set_state("b", JobState::Flagged).unwrap();

        assert_eq!(ledger.count_jobs(Some("acme"), None).unwrap(), 2);
        assert_eq!(
            ledger.search_jobs(Some("acme"), None, 10, 0).unwrap().len(),
            2
        );
        assert_eq!(
            ledger.search_jobs(Some("acme"), None, 1, 0).unwrap().len(),
            1
        );
        assert_eq!(
            ledger.search_jobs(Some("acme"), None, 10, 2).unwrap().len(),
            0
        );
        assert_eq!(ledger.count_jobs(None, Some(JobState::Flagged)).unwrap(), 1);
        assert_eq!(
            ledger
                .list_by_state_paged(JobState::Flagged, 10, 0)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            ledger
                .list_by_state_paged(JobState::Flagged, 10, 1)
                .unwrap()
                .len(),
            0
        );

        // A wildcard typed by a human is a literal, not a match-everything.
        assert_eq!(ledger.count_jobs(Some("%"), None).unwrap(), 0);
    }

    #[test]
    fn reset_for_reprocess_clears_every_derived_field() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "retry");
        ledger
            .reserve_name("2024-02-02 Something Named", "pdf", "retry")
            .unwrap();
        ledger
            .update_fields(
                "retry",
                &[
                    ("flag_reason", Some("SLM_FAIL:no valid output".into())),
                    ("quarantine_path", Some("Q:/q/doc.pdf".into())),
                    ("proposed_subject", Some("Something Named".into())),
                ],
            )
            .unwrap();
        ledger.set_state("retry", JobState::Flagged).unwrap();
        ledger.try_claim("retry", 600).unwrap();

        assert!(ledger.reset_for_reprocess("retry").unwrap());
        let job = ledger.get("retry").unwrap().unwrap();
        assert_eq!(job.state, JobState::Ingested);
        assert_eq!(job.attempts, 0);
        assert!(job.flag_reason.is_none());
        assert!(job.final_filename.is_none());
        assert!(job.proposed_subject.is_none());
        assert!(job.quarantine_path.is_none());
        assert!(job.claimed_at.is_none());
        assert!(!ledger.reset_for_reprocess("ghost").unwrap());
    }

    #[test]
    fn throughput_counts_only_terminal_jobs_in_the_window() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "t1");
        seed(&ledger, "t2");
        seed(&ledger, "t3");
        ledger.set_state("t1", JobState::Emitted).unwrap();
        ledger.set_state("t2", JobState::Flagged).unwrap();
        assert_eq!(ledger.throughput(60).unwrap(), 2);
        // A zero-width window is entirely in the past.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(ledger.throughput(0).unwrap(), 0);
    }

    #[test]
    fn update_fields_writes_the_whole_slice_and_rejects_bad_columns() {
        let (_dir, ledger) = ledger();
        seed(&ledger, "kv");
        ledger
            .update_fields(
                "kv",
                &[
                    ("doc_type", Some("invoice".into())),
                    ("language", Some("en".into())),
                    ("flag_reason", None),
                ],
            )
            .unwrap();
        let job = ledger.get("kv").unwrap().unwrap();
        assert_eq!(job.doc_type.as_deref(), Some("invoice"));
        assert_eq!(job.language.as_deref(), Some("en"));
        assert!(job.flag_reason.is_none());

        assert!(ledger
            .update_fields("kv", &[("state", Some("emitted".into()))])
            .is_err());
        assert!(ledger
            .update_fields("kv", &[("doc_type", None), ("doc_type", None)])
            .is_err());
        ledger.update_fields("kv", &[]).unwrap();
    }
}
