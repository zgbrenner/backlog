//! Physical-delivery statistics for the desktop header.
//!
//! Content jobs are deduplicated by SHA-256, so counting the `jobs` table can
//! under-report several byte-identical files that are independently processing,
//! emitted, or awaiting review. New ledgers therefore report `file_instances`.
//! A legacy jobs fallback keeps pre-migration ledgers readable until the watcher
//! registers their physical deliveries.

use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use std::time::Duration;

pub fn snapshot(db_path: &Path) -> anyhow::Result<serde_json::Value> {
    let connection = Connection::open(db_path)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let instance_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM file_instances",
        [],
        |row| row.get(0),
    )?;
    let (table, state_column) = if instance_count > 0 {
        ("file_instances", "state")
    } else {
        ("jobs", "state")
    };

    let mut statement = connection.prepare(&format!(
        "SELECT {state_column}, COUNT(*) FROM {table} GROUP BY {state_column}"
    ))?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut values = serde_json::Map::new();
    for row in rows {
        let (state, count) = row?;
        values.insert(state, serde_json::json!(count));
    }
    Ok(serde_json::Value::Object(values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{InstanceState, JobState, Ledger};

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FIRST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SECOND: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    #[test]
    fn duplicate_physical_files_are_counted_separately() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        ledger.ingest(SHA, "one.pdf", "one.pdf", "pdf").unwrap();
        ledger.set_state(SHA, JobState::Flagged).unwrap();
        for instance in [FIRST, SECOND] {
            ledger
                .register_instance(instance, SHA, instance, "one.pdf", "pdf")
                .unwrap();
            ledger
                .set_instance_state(instance, InstanceState::Flagged)
                .unwrap();
        }

        let value = snapshot(&db_path).unwrap();
        assert_eq!(value["flagged"], 2);
        assert!(value.get("emitted").is_none());
    }

    #[test]
    fn legacy_content_jobs_are_used_before_instances_exist() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        ledger.ingest(SHA, "one.pdf", "one.pdf", "pdf").unwrap();
        ledger.set_state(SHA, JobState::Emitted).unwrap();

        let value = snapshot(&db_path).unwrap();
        assert_eq!(value["emitted"], 1);
    }
}
