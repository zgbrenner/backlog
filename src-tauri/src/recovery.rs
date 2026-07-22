//! Reliability wrapper around the core pipeline.
//!
//! The core orchestrator remains focused on document work. Watcher-delivered
//! jobs enter through `process_file_recoverable`, which makes pause durable,
//! enforces the configured wall-clock cap, and records a terminal timeout only
//! after a manifest is durable.

use crate::identity::{instance_id as derive_instance_id, normalize_relpath};
use crate::ledger::{InstanceState, JobState};
use crate::manifest::{write_manifest, Manifest, MANIFEST_SCHEMA_VERSION};
use crate::pipeline::{hash_file, Pipeline};
use crate::routing;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::Emitter;

impl Pipeline {
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub async fn process_file_recoverable(self: Arc<Self>, path: PathBuf) {
        loop {
            wait_for_resume(&self.paused).await;
            if !path.exists() {
                return;
            }

            let result = tokio::time::timeout(
                Duration::from_secs(self.cfg.per_file_wall_clock_secs),
                self.clone().process_file(path.clone()),
            )
            .await;

            if result.is_err() {
                self.sidecar.terminate();
                self.slm.shutdown();
                self.record_wall_clock_timeout(&path).await;
                return;
            }

            // Close the small race where pause is enabled after this wrapper
            // wakes but before Pipeline::process_file checks its atomic flag.
            // The core returns immediately in that case, so wait and replay the
            // same still-present file instead of consuming its only event.
            if self.is_paused() && path.exists() {
                continue;
            }
            return;
        }
    }

    async fn record_wall_clock_timeout(&self, path: &Path) {
        let sha256 = match hash_file(path) {
            Ok(hash) => hash,
            Err(error) => {
                log::error!(
                    "RUNTIME_TIMEOUT for {}, but recovery hashing failed: {error}",
                    path.display()
                );
                return;
            }
        };
        let original_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
        let original_relpath = relative_path(&self.cfg.processing_dir, path);
        let instance_id = derive_instance_id(&sha256, &normalize_relpath(&original_relpath));
        let ext = routing::extension_of(path);

        match self.ledger.register_instance(
            &instance_id,
            &sha256,
            &path.to_string_lossy(),
            &original_name,
            &ext,
        ) {
            Ok(Some(existing)) if existing.state.is_terminal() => return,
            Ok(_) => {}
            Err(error) => {
                log::error!("timeout instance registration failed: {error}");
                return;
            }
        }

        let duplicate = self
            .ledger
            .get(&sha256)
            .ok()
            .flatten()
            .is_some_and(|job| !same_path(&job.original_path, path));
        let reason = format!(
            "RUNTIME_TIMEOUT:exceeded {} seconds",
            self.cfg.per_file_wall_clock_secs
        );
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: instance_id.clone(),
            sha256: sha256.clone(),
            status: "flagged".into(),
            original_name: original_name.clone(),
            original_relpath,
            new_filename: None,
            description: None,
            date: None,
            date_source: None,
            doc_type: None,
            language: None,
            duplicate_of: duplicate.then(|| sha256.clone()),
            soft_flags: duplicate
                .then(|| vec!["DUPLICATE_CONTENT".into()])
                .unwrap_or_default(),
            flag_reason: Some(reason.clone()),
            model_versions: json!({}),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };

        let quarantine_error = match persist_flagged_before_quarantine(
            &self.cfg.manifests_dir(),
            &self.cfg.quarantine_dir,
            path,
            &original_name,
            &instance_id,
            &manifest,
        ) {
            Ok(error) => error,
            Err(error) => {
                let _ = self.ledger.log_event(
                    &sha256,
                    "timeout",
                    &format!("manifest write failed: {error}"),
                );
                log::error!(
                    "timeout manifest failed for instance {instance_id}; source remains recoverable: {error}"
                );
                return;
            }
        };

        let _ = self
            .ledger
            .update_fields(&sha256, &[("flag_reason", Some(reason.clone()))]);
        let _ = self.ledger.set_state(&sha256, JobState::Flagged);
        let _ = self
            .ledger
            .set_instance_state(&instance_id, InstanceState::Flagged);
        let detail = match quarantine_error {
            Some(error) => format!("{reason}; quarantine failed: {error}"),
            None => reason,
        };
        let _ = self.ledger.log_event(&sha256, "timeout", &detail);
        if let Ok(Some(job)) = self.ledger.get(&sha256) {
            let _ = self.app.emit("job-updated", &job);
        }
    }
}

async fn wait_for_resume(paused: &AtomicBool) {
    while paused.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A manifest failure returns before quarantine is touched. Quarantine errors
/// after a durable manifest are returned as data so the caller can still mark
/// the instance terminal and surface the operational problem.
fn persist_flagged_before_quarantine(
    manifests_dir: &Path,
    quarantine_dir: &Path,
    source: &Path,
    original_name: &str,
    instance_id: &str,
    manifest: &Manifest,
) -> anyhow::Result<Option<String>> {
    write_manifest(manifests_dir, manifest)?;
    Ok(quarantine_source(
        quarantine_dir,
        source,
        original_name,
        instance_id,
    )
    .err()
    .map(|error| error.to_string()))
}

fn quarantine_source(
    quarantine_dir: &Path,
    source: &Path,
    original_name: &str,
    instance_id: &str,
) -> anyhow::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(quarantine_dir)?;
    let mut destination = quarantine_dir.join(original_name);
    if destination.exists() {
        destination = quarantine_dir.join(format!("{}-{original_name}", &instance_id[..12]));
    }

    match std::fs::rename(source, &destination) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            std::fs::copy(source, &destination).map_err(|copy_error| {
                anyhow::anyhow!(
                    "rename failed ({rename_error}); quarantine copy failed ({copy_error})"
                )
            })?;
            if let Err(remove_error) = std::fs::remove_file(source) {
                log::warn!(
                    "quarantine copy is durable at {}, but source cleanup failed: {remove_error}",
                    destination.display()
                );
            }
            Ok(())
        }
    }
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn same_path(existing: &str, current: &Path) -> bool {
    normalize_relpath(existing) == normalize_relpath(&current.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const INSTANCE: &str =
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn flagged_manifest() -> Manifest {
        Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: INSTANCE.into(),
            sha256: SHA.into(),
            status: "flagged".into(),
            original_name: "slow.pdf".into(),
            original_relpath: "slow.pdf".into(),
            new_filename: None,
            description: None,
            date: None,
            date_source: None,
            doc_type: None,
            language: None,
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: Some("RUNTIME_TIMEOUT:test".into()),
            model_versions: json!({}),
            processed_at: "2026-07-21T12:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn pause_waiter_does_not_consume_delivery() {
        let paused = Arc::new(AtomicBool::new(true));
        let waiter = paused.clone();
        let task = tokio::spawn(async move {
            wait_for_resume(&waiter).await;
            "resumed"
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!task.is_finished());
        paused.store(false, Ordering::Release);

        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("waiter should wake")
            .expect("waiter should not panic");
        assert_eq!(result, "resumed");
    }

    #[test]
    fn manifest_failure_leaves_source_in_processing() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("slow.pdf");
        std::fs::write(&source, b"fixture").unwrap();
        let blocker = dir.path().join("manifest-blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let quarantine = dir.path().join("quarantine");

        let result = persist_flagged_before_quarantine(
            &blocker.join("nested"),
            &quarantine,
            &source,
            "slow.pdf",
            INSTANCE,
            &flagged_manifest(),
        );

        assert!(result.is_err());
        assert!(source.exists());
        assert!(!quarantine.exists());
    }
}
