//! Manifest v2 handoff: one JSON manifest per physical file instance into
//! `<outbox>/_manifests`, where Power Automate Flow 2 consumes it.
//!
//! The content SHA-256 identifies bytes. `manifest_id` identifies a stable
//! physical-file delivery and is therefore the idempotency key and filename.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MANIFEST_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub manifest_id: String,
    pub sha256: String,
    pub status: String, // "ok" | "flagged"
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
        anyhow::ensure!(!self.original_name.trim().is_empty(), "missing original_name");
        anyhow::ensure!(
            !self.original_relpath.trim().is_empty(),
            "missing original_relpath"
        );
        anyhow::ensure!(!self.processed_at.trim().is_empty(), "missing processed_at");

        match self.status.as_str() {
            "ok" => {
                anyhow::ensure!(self.new_filename.is_some(), "ok manifest missing new_filename");
                anyhow::ensure!(self.description.is_some(), "ok manifest missing description");
                anyhow::ensure!(self.date.is_some(), "ok manifest missing date");
                anyhow::ensure!(self.flag_reason.is_none(), "ok manifest cannot have flag_reason");
                if let Some(filename) = &self.new_filename {
                    anyhow::ensure!(
                        !filename.contains(['/', '\\']) && filename != "." && filename != "..",
                        "new_filename must be one safe path component"
                    );
                }
            }
            "flagged" => {
                anyhow::ensure!(
                    self.flag_reason
                        .as_deref()
                        .is_some_and(|reason| !reason.trim().is_empty()),
                    "flagged manifest missing flag_reason"
                );
                anyhow::ensure!(
                    self.new_filename.is_none(),
                    "flagged manifest cannot have new_filename"
                );
            }
            other => anyhow::bail!("invalid manifest status '{other}'"),
        }

        Ok(())
    }
}

/// Write a validated manifest atomically. Replaying an identical manifest is a
/// no-op; attempting to reuse a manifest ID for different content fails closed.
pub fn write_manifest(dir: &Path, manifest: &Manifest) -> anyhow::Result<PathBuf> {
    manifest.validate()?;
    std::fs::create_dir_all(dir)?;

    let final_path = dir.join(format!("{}.json", manifest.manifest_id));
    let bytes = serde_json::to_vec_pretty(manifest)?;

    if final_path.exists() {
        let existing = std::fs::read(&final_path)?;
        anyhow::ensure!(
            same_delivery(&existing, manifest)?,
            "manifest ID {} already exists with different content",
            manifest.manifest_id
        );
        return Ok(final_path);
    }

    let tmp_path = dir.join(format!(
        ".{}.{}.json.tmp",
        manifest.manifest_id,
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = match options.open(&tmp_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&tmp_path)?;
            options.open(&tmp_path)?
        }
        Err(error) => return Err(error.into()),
    };
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);

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
