//! Manifest v3 handoff: one JSON manifest per physical file instance into
//! `<outbox>/_manifests`, where Power Automate Flow 2 consumes it.
//!
//! The content SHA-256 identifies bytes. `manifest_id` identifies a stable
//! physical-file delivery and is therefore the idempotency key and filename.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bumped to 3 for the `dismissed` status and the non-empty `model_versions`
/// requirement on `ok`. `power-automate/manifest.schema.json`,
/// `manifest.parse-json.schema.json` and `FLOW2-commit.md` describe the same
/// contract from the SharePoint side and move with this number.
pub const MANIFEST_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub manifest_id: String,
    pub sha256: String,
    /// `"ok"` | `"flagged"` | `"dismissed"`. `dismissed` is a terminal human
    /// decision — "this is junk, stop showing it to me" — and maps to the
    /// `ReviewState = Dismissed` the downstream list already defines but the
    /// app previously had no way to produce.
    pub status: String,
    pub original_name: String,
    pub original_relpath: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub soft_flags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag_reason: Option<String>,
    pub model_versions: serde_json::Value,
    pub processed_at: String,
}

impl Manifest {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == MANIFEST_SCHEMA_VERSION,
            "unsupported manifest schema {}",
            self.schema
        );
        anyhow::ensure!(
            crate::identity::is_safe_identifier(&self.manifest_id),
            "unsafe manifest identifier"
        );
        anyhow::ensure!(
            crate::identity::is_safe_identifier(&self.sha256),
            "invalid content SHA-256"
        );
        if let Some(duplicate_of) = &self.duplicate_of {
            anyhow::ensure!(
                crate::identity::is_safe_identifier(duplicate_of),
                "duplicate_of must be a content SHA-256"
            );
        }
        anyhow::ensure!(
            !self.original_name.trim().is_empty(),
            "missing original_name"
        );
        anyhow::ensure!(
            !self.original_relpath.trim().is_empty(),
            "missing original_relpath"
        );
        anyhow::ensure!(!self.processed_at.trim().is_empty(), "missing processed_at");
        anyhow::ensure!(
            self.model_versions.is_object(),
            "model_versions must be an object"
        );

        match self.status.as_str() {
            "ok" => {
                anyhow::ensure!(
                    self.new_filename.is_some(),
                    "ok manifest missing new_filename"
                );
                anyhow::ensure!(
                    self.description.is_some(),
                    "ok manifest missing description"
                );
                anyhow::ensure!(self.date.is_some(), "ok manifest missing date");
                anyhow::ensure!(
                    self.date_source.is_some(),
                    "ok manifest missing date_source"
                );
                anyhow::ensure!(
                    self.flag_reason.is_none(),
                    "ok manifest cannot have flag_reason"
                );
                if let Some(filename) = &self.new_filename {
                    anyhow::ensure!(
                        !filename.contains('/')
                            && !filename.contains('\\')
                            && filename != "."
                            && filename != "..",
                        "new_filename must be one safe path component"
                    );
                }
                // The ledger's whole promise is that an emitted name is
                // reproducible, and `model_versions` is the only record of
                // what produced it. An empty object means the sidecar was
                // never successfully asked — a transient hiccup at pipeline
                // construction would otherwise void that promise silently,
                // for an entire batch, with nothing anywhere saying so.
                anyhow::ensure!(
                    self.model_versions
                        .as_object()
                        .is_some_and(|versions| !versions.is_empty()),
                    "ok manifest has no model provenance (model_versions is empty)"
                );
            }
            // A dismissal carries the operator's note in `flag_reason` and,
            // like a flag, never renames anything — the two share their
            // required fields exactly.
            "flagged" | "dismissed" => {
                anyhow::ensure!(
                    self.flag_reason
                        .as_deref()
                        .is_some_and(|reason| !reason.trim().is_empty()),
                    "{} manifest missing flag_reason",
                    self.status
                );
                anyhow::ensure!(
                    self.new_filename.is_none(),
                    "{} manifest cannot have new_filename",
                    self.status
                );
            }
            other => anyhow::bail!("invalid manifest status '{other}'"),
        }

        Ok(())
    }
}

/// Write a validated manifest atomically. Replaying an identical manifest is a
/// no-op; attempting to reuse a manifest ID for different content fails closed.
/// A matching pending `flagged` delivery may transition to `ok` (corrected) or
/// `dismissed` (judged junk) after human review through the guarded
/// replacement path below.
pub fn write_manifest(dir: &Path, manifest: &Manifest) -> anyhow::Result<PathBuf> {
    manifest.validate()?;
    std::fs::create_dir_all(dir)?;

    let final_path = manifest_path(dir, &manifest.manifest_id);
    let bytes = serde_json::to_vec_pretty(manifest)?;

    if final_path.exists() {
        let existing_bytes = std::fs::read(&final_path)?;
        let existing: Manifest = serde_json::from_slice(&existing_bytes)?;
        if existing.status == "flagged" && manifest.status != "flagged" {
            return replace_flagged_manifest(dir, manifest);
        }
        anyhow::ensure!(
            same_delivery(&existing_bytes, manifest)?,
            "manifest ID {} already exists with different content",
            manifest.manifest_id
        );
        return Ok(final_path);
    }

    let tmp_path = temporary_path(dir, &manifest.manifest_id, "write");
    write_synced_temp(&tmp_path, &bytes)?;

    if let Err(rename_error) = std::fs::rename(&tmp_path, &final_path) {
        if final_path.exists() && same_delivery(&std::fs::read(&final_path)?, manifest)? {
            let _ = std::fs::remove_file(&tmp_path);
            return Ok(final_path);
        }
        let _ = std::fs::remove_file(&tmp_path);
        return Err(rename_error.into());
    }

    Ok(final_path)
}

/// Replace one still-pending flagged delivery after a human has ruled on it:
/// `ok` when they corrected the metadata, `dismissed` when they decided the
/// file is not worth filing. The stable manifest ID, true content SHA, and
/// physical source identity must all match, and flagged is the only state
/// that may be left — a delivery never regresses back into review.
pub fn replace_flagged_manifest(dir: &Path, corrected: &Manifest) -> anyhow::Result<PathBuf> {
    corrected.validate()?;
    anyhow::ensure!(
        corrected.status == "ok" || corrected.status == "dismissed",
        "replacement manifest must have status 'ok' or 'dismissed', got '{}'",
        corrected.status
    );
    std::fs::create_dir_all(dir)?;

    let final_path = manifest_path(dir, &corrected.manifest_id);
    let backup_path = dir.join(format!(".{}.flagged.bak", corrected.manifest_id));
    recover_stale_backup(&final_path, &backup_path)?;
    if !final_path.exists() {
        return write_manifest(dir, corrected);
    }

    let existing_bytes = std::fs::read(&final_path)?;
    let existing: Manifest = serde_json::from_slice(&existing_bytes)?;
    existing.validate()?;
    anyhow::ensure!(
        existing.status == "flagged",
        "only a flagged manifest may be replaced"
    );
    anyhow::ensure!(
        existing.manifest_id == corrected.manifest_id,
        "replacement manifest ID mismatch"
    );
    anyhow::ensure!(
        existing.sha256 == corrected.sha256,
        "replacement content SHA-256 mismatch"
    );
    anyhow::ensure!(
        existing.original_name == corrected.original_name
            && existing.original_relpath == corrected.original_relpath,
        "replacement source identity mismatch"
    );

    let corrected_bytes = serde_json::to_vec_pretty(corrected)?;
    let tmp_path = temporary_path(dir, &corrected.manifest_id, "review");
    write_synced_temp(&tmp_path, &corrected_bytes)?;
    std::fs::rename(&final_path, &backup_path)?;

    if let Err(replace_error) = std::fs::rename(&tmp_path, &final_path) {
        let restore_result = std::fs::rename(&backup_path, &final_path);
        let _ = std::fs::remove_file(&tmp_path);
        if let Err(restore_error) = restore_result {
            anyhow::bail!(
                "manifest replacement failed ({replace_error}) and backup restore failed ({restore_error})"
            );
        }
        return Err(replace_error.into());
    }

    if let Err(error) = std::fs::remove_file(&backup_path) {
        log::warn!("corrected manifest was committed, but stale backup cleanup failed: {error}");
    }
    Ok(final_path)
}

fn manifest_path(dir: &Path, manifest_id: &str) -> PathBuf {
    dir.join(format!("{manifest_id}.json"))
}

fn temporary_path(dir: &Path, manifest_id: &str, purpose: &str) -> PathBuf {
    dir.join(format!(
        ".{manifest_id}.{}.{purpose}.json.tmp",
        std::process::id()
    ))
}

fn write_synced_temp(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

fn recover_stale_backup(final_path: &Path, backup_path: &Path) -> anyhow::Result<()> {
    if !backup_path.exists() {
        return Ok(());
    }
    if final_path.exists() {
        std::fs::remove_file(backup_path)?;
    } else {
        std::fs::rename(backup_path, final_path)?;
    }
    Ok(())
}

fn same_delivery(existing_bytes: &[u8], proposed: &Manifest) -> anyhow::Result<bool> {
    let existing: Manifest = serde_json::from_slice(existing_bytes)?;
    let mut normalized_proposed = proposed.clone();
    normalized_proposed.processed_at = existing.processed_at.clone();
    Ok(existing == normalized_proposed)
}

/// Token-bucket pacer for manifest emission (0 = unlimited).
pub struct Pacer {
    per_min: u32,
    last_emit: std::sync::Mutex<Vec<std::time::Instant>>,
}

impl Pacer {
    pub fn new(per_min: u32) -> Self {
        Self {
            per_min,
            last_emit: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub async fn permit(&self) {
        if self.per_min == 0 {
            return;
        }
        loop {
            let wait = {
                let mut values = self.last_emit.lock().unwrap();
                let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(60);
                values.retain(|time| *time > cutoff);
                if (values.len() as u32) < self.per_min {
                    values.push(std::time::Instant::now());
                    None
                } else {
                    Some(std::time::Duration::from_millis(1500))
                }
            };
            match wait {
                None => return,
                Some(duration) => tokio::time::sleep(duration).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base(status: &str) -> Manifest {
        Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: "a".repeat(64),
            sha256: "b".repeat(64),
            status: status.to_string(),
            original_name: "scan.pdf".into(),
            original_relpath: "sub/scan.pdf".into(),
            new_filename: None,
            description: None,
            date: None,
            date_source: None,
            doc_type: Some("correspondence".into()),
            language: Some("en".into()),
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: None,
            model_versions: json!({ "convertd": "0.2.0" }),
            processed_at: "2026-07-28T09:00:00Z".into(),
        }
    }

    fn ok_manifest() -> Manifest {
        Manifest {
            new_filename: Some("2026-01-09 Lease Renewal Notice.pdf".into()),
            description: Some("Landlord confirms the lease renews for another year.".into()),
            date: Some("2026-01-09".into()),
            date_source: Some("document".into()),
            ..base("ok")
        }
    }

    fn dismissed_manifest() -> Manifest {
        Manifest {
            flag_reason: Some("DISMISSED:cover sheet, nothing to file".into()),
            ..base("dismissed")
        }
    }

    fn flagged_manifest() -> Manifest {
        Manifest {
            flag_reason: Some("SLM_FAIL".into()),
            ..base("flagged")
        }
    }

    #[test]
    fn accepts_a_well_formed_dismissed_manifest() {
        dismissed_manifest()
            .validate()
            .expect("a dismissal is a valid terminal outcome");
    }

    #[test]
    fn a_dismissed_manifest_still_needs_a_reason_and_still_renames_nothing() {
        let mut without_reason = dismissed_manifest();
        without_reason.flag_reason = Some("   ".into());
        assert!(without_reason.validate().is_err());

        let mut renamed = dismissed_manifest();
        renamed.new_filename = Some("2026-01-09 Something.pdf".into());
        assert!(renamed.validate().is_err());
    }

    /// `model_versions` is snapshotted once at pipeline construction; an empty
    /// object means the sidecar never answered, so nothing that claims to be
    /// reproducible may ship with one.
    #[test]
    fn rejects_an_ok_manifest_with_empty_model_versions() {
        let mut manifest = ok_manifest();
        manifest.model_versions = json!({});
        let error = manifest
            .validate()
            .expect_err("an ok manifest without provenance must not ship");
        assert!(error.to_string().contains("model_versions"), "{error}");

        // A flagged or dismissed delivery makes no reproducibility claim, so
        // it is not held to the same bar.
        let mut flagged = flagged_manifest();
        flagged.model_versions = json!({});
        flagged.validate().unwrap();
    }

    #[test]
    fn a_well_formed_ok_manifest_still_validates() {
        ok_manifest().validate().unwrap();
    }

    #[test]
    fn every_declared_status_is_one_validate_accepts() {
        for status in ["ok", "flagged", "dismissed"] {
            let manifest = match status {
                "ok" => ok_manifest(),
                "dismissed" => dismissed_manifest(),
                _ => flagged_manifest(),
            };
            manifest
                .validate()
                .unwrap_or_else(|e| panic!("{status}: {e}"));
        }
        let mut unknown = flagged_manifest();
        unknown.status = "withdrawn".into();
        assert!(unknown.validate().is_err());
    }

    /// The dismiss command acts on files that are already flagged, so the
    /// pending flagged manifest in the Outbox has to be replaceable by the
    /// dismissal — otherwise Needs Review still has no terminal state.
    #[test]
    fn a_pending_flagged_delivery_can_be_replaced_by_a_dismissal() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), &flagged_manifest()).unwrap();

        let replaced = write_manifest(dir.path(), &dismissed_manifest()).unwrap();
        assert_eq!(replaced, path);

        let on_disk: Manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk.status, "dismissed");
        assert!(!dir
            .path()
            .join(format!(".{}.flagged.bak", on_disk.manifest_id))
            .exists());
    }

    #[test]
    fn a_delivery_never_regresses_back_into_review() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), &dismissed_manifest()).unwrap();
        assert!(
            write_manifest(dir.path(), &flagged_manifest()).is_err(),
            "a dismissed delivery must not be reopened by a new flag"
        );
    }

    #[test]
    fn replacement_refuses_a_status_that_is_not_terminal() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), &flagged_manifest()).unwrap();
        let error = replace_flagged_manifest(dir.path(), &flagged_manifest())
            .expect_err("flagged is not a terminal state");
        assert!(error.to_string().contains("'ok' or 'dismissed'"), "{error}");
    }

    #[test]
    fn manifests_from_an_older_schema_are_refused() {
        let mut stale = ok_manifest();
        stale.schema = MANIFEST_SCHEMA_VERSION - 1;
        assert!(stale.validate().is_err());
    }
}
