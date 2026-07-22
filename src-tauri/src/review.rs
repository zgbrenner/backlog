//! Instance-aware human review and corrected-manifest emission.
//!
//! Content jobs are keyed by SHA-256, but review must not collapse several
//! flagged physical deliveries into one row. This module queries flagged file
//! instances, overlays a still-present flagged manifest, restores the matching
//! quarantined source, and re-emits one corrected manifest with the same stable
//! ManifestId.

use crate::checker::{Checker, SlmOutput, Validated};
use crate::config::Config;
use crate::identity::{instance_id as derive_instance_id, is_safe_identifier, normalize_relpath};
use crate::ledger::{FileInstance, InstanceState, Job, JobState, Ledger};
use crate::manifest::{write_manifest, Manifest, MANIFEST_SCHEMA_VERSION};
use crate::pipeline::{hash_file, Pipeline};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tauri::Emitter;

#[derive(Debug, Clone, Serialize)]
pub struct ReviewItem {
    pub instance_id: String,
    pub sha256: String,
    pub original_name: String,
    pub flag_reason: Option<String>,
    pub proposed_date: Option<String>,
    pub proposed_subject: Option<String>,
    pub description: Option<String>,
    pub doc_type: Option<String>,
    pub soft_flags: Option<String>,
    pub updated_at: String,
}

pub fn list_review_items(
    db_path: &Path,
    manifests_dir: &Path,
    limit: usize,
) -> anyhow::Result<Vec<ReviewItem>> {
    let connection = Connection::open(db_path)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let mut statement = connection.prepare(
        "SELECT fi.instance_id,
                fi.sha256,
                fi.original_name,
                j.flag_reason,
                j.proposed_date,
                j.proposed_subject,
                j.description,
                j.doc_type,
                j.soft_flags,
                fi.updated_at
         FROM file_instances AS fi
         LEFT JOIN jobs AS j ON j.sha256 = fi.sha256
         WHERE fi.state = 'flagged'
         ORDER BY fi.updated_at DESC
         LIMIT ?1",
    )?;

    let rows = statement.query_map(params![limit.clamp(1, 5000) as i64], |row| {
        Ok(ReviewItem {
            instance_id: row.get(0)?,
            sha256: row.get(1)?,
            original_name: row.get(2)?,
            flag_reason: row.get(3)?,
            proposed_date: row.get(4)?,
            proposed_subject: row.get(5)?,
            description: row.get(6)?,
            doc_type: row.get(7)?,
            soft_flags: row.get(8)?,
            updated_at: row.get(9)?,
        })
    })?;

    let mut items = Vec::new();
    for row in rows {
        let mut item = row?;
        match read_flagged_manifest(manifests_dir, &item.instance_id, &item.sha256) {
            Ok(Some(manifest)) => {
                item.flag_reason = manifest.flag_reason;
                if !manifest.soft_flags.is_empty() {
                    item.soft_flags = Some(manifest.soft_flags.join(","));
                }
            }
            Ok(None) => {}
            Err(error) => {
                item.flag_reason = Some(format!("REVIEW_MANIFEST_ERROR:{error}"));
            }
        }
        items.push(item);
    }
    Ok(items)
}

impl Pipeline {
    pub async fn resubmit_instance(
        &self,
        instance_id: &str,
        date: String,
        subject: String,
        description: String,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(is_safe_identifier(instance_id), "invalid review InstanceId");
        let instance = self
            .ledger
            .instance(instance_id)?
            .ok_or_else(|| anyhow::anyhow!("unknown file instance"))?;
        anyhow::ensure!(
            instance.state == InstanceState::Flagged,
            "file instance is not pending review"
        );
        let job = self
            .ledger
            .get(&instance.sha256)?
            .ok_or_else(|| anyhow::anyhow!("content job is missing"))?;

        let markdown = std::fs::read_to_string(
            self.cfg.cache_dir.join(format!("{}.md", instance.sha256)),
        )
        .unwrap_or_default();
        let harvest = crate::harvest::harvest(&markdown);
        let checker = Checker::new(self.cfg.max_filename_len);
        let output = SlmOutput {
            date: date.clone(),
            date_source: "document".into(),
            subject,
            description,
        };
        let today = chrono::Utc::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let validated = checker.check(&output, &harvest, &[date], &today, None)?;

        let owner_id = content_owner_instance_id(&self.cfg.processing_dir, &job);
        self.ledger.register_instance(
            &owner_id,
            &job.sha256,
            &job.original_path,
            &job.original_name,
            &job.ext,
        )?;
        let owner_filename = self
            .ledger
            .reserve_filename(&owner_id, &validated.base_name, &job.ext)?;
        let duplicate = owner_id != instance.instance_id;
        let final_filename = if duplicate {
            self.ledger.reserve_filename(
                &instance.instance_id,
                &validated.base_name,
                &instance.ext,
            )?
        } else {
            owner_filename.clone()
        };

        let pending_manifest = read_flagged_manifest(
            &self.cfg.manifests_dir(),
            &instance.instance_id,
            &instance.sha256,
        )?;
        let original_relpath = original_relpath_for_review(
            &self.cfg,
            &instance,
            pending_manifest.as_ref(),
        )?;
        restore_source_for_instance(&self.cfg, &instance, &original_relpath)?;

        let corrected = corrected_manifest(
            &instance,
            &job,
            &validated,
            final_filename,
            original_relpath,
            corrected_instance_flags(&job, duplicate),
            duplicate,
        );
        write_manifest(&self.cfg.manifests_dir(), &corrected)?;

        let mut content_flags = validated.soft_flags.clone();
        push_unique(&mut content_flags, "HUMAN_CORRECTED");
        self.ledger.update_fields(
            &job.sha256,
            &[
                ("proposed_date", Some(validated.date_iso.clone())),
                ("date_source", Some("human".into())),
                ("proposed_subject", Some(validated.subject.clone())),
                ("description", Some(validated.description.clone())),
                ("final_filename", Some(owner_filename)),
                ("flag_reason", None),
                ("soft_flags", Some(content_flags.join(","))),
            ],
        )?;
        self.ledger.set_state(&job.sha256, JobState::Emitted)?;
        self.ledger
            .set_instance_state(&instance.instance_id, InstanceState::Emitted)?;
        self.ledger.log_event(
            &job.sha256,
            "resubmit",
            &format!(
                "human correction accepted for instance {} duplicate={duplicate}",
                instance.instance_id
            ),
        )?;
        if let Some(updated) = self.ledger.get(&job.sha256)? {
            let _ = self.app.emit("job-updated", &updated);
        }
        Ok(())
    }
}

fn read_flagged_manifest(
    manifests_dir: &Path,
    instance_id: &str,
    sha256: &str,
) -> anyhow::Result<Option<Manifest>> {
    let path = manifests_dir.join(format!("{instance_id}.json"));
    if !path.is_file() {
        return Ok(None);
    }
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(path)?)?;
    manifest.validate()?;
    anyhow::ensure!(
        manifest.manifest_id == instance_id && manifest.sha256 == sha256,
        "pending review manifest does not match the ledger instance"
    );
    Ok((manifest.status == "flagged").then_some(manifest))
}

fn content_owner_instance_id(processing_dir: &Path, job: &Job) -> String {
    let relative = relative_path(processing_dir, Path::new(&job.original_path));
    derive_instance_id(&job.sha256, &normalize_relpath(&relative))
}

fn original_relpath_for_review(
    cfg: &Config,
    instance: &FileInstance,
    pending_manifest: Option<&Manifest>,
) -> anyhow::Result<String> {
    let relative = pending_manifest
        .map(|manifest| manifest.original_relpath.clone())
        .unwrap_or_else(|| relative_path(&cfg.processing_dir, Path::new(&instance.original_path)));
    anyhow::ensure!(safe_relative_path(&relative), "unsafe review source path");
    Ok(relative.replace('\\', "/"))
}

fn safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.trim().is_empty()
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(component, Component::Normal(_) | Component::CurDir)
        })
}

fn restore_source_for_instance(
    cfg: &Config,
    instance: &FileInstance,
    original_relpath: &str,
) -> anyhow::Result<PathBuf> {
    let destination = cfg.processing_dir.join(original_relpath);
    if destination.is_file() {
        anyhow::ensure!(
            hash_file(&destination)? == instance.sha256,
            "review destination already contains different bytes"
        );
        return Ok(destination);
    }

    let specific = cfg.quarantine_dir.join(format!(
        "{}-{}",
        &instance.instance_id[..12],
        instance.original_name
    ));
    let ordinary = cfg.quarantine_dir.join(&instance.original_name);
    let source = [specific, ordinary]
        .into_iter()
        .find(|candidate| {
            candidate.is_file()
                && hash_file(candidate)
                    .map(|digest| digest == instance.sha256)
                    .unwrap_or(false)
        })
        .ok_or_else(|| anyhow::anyhow!("the matching quarantined source could not be found"))?;

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(&source, &destination) {
        Ok(()) => {}
        Err(rename_error) => {
            std::fs::copy(&source, &destination).map_err(|copy_error| {
                anyhow::anyhow!(
                    "review restore rename failed ({rename_error}); copy failed ({copy_error})"
                )
            })?;
            std::fs::remove_file(&source)?;
        }
    }
    Ok(destination)
}

fn corrected_instance_flags(job: &Job, duplicate: bool) -> Vec<String> {
    let mut flags = parse_flags(job.soft_flags.as_deref());
    push_unique(&mut flags, "HUMAN_CORRECTED");
    if duplicate {
        push_unique(&mut flags, "DUPLICATE_CONTENT");
    }
    flags
}

#[allow(clippy::too_many_arguments)]
fn corrected_manifest(
    instance: &FileInstance,
    job: &Job,
    validated: &Validated,
    final_filename: String,
    original_relpath: String,
    soft_flags: Vec<String>,
    duplicate: bool,
) -> Manifest {
    Manifest {
        schema: MANIFEST_SCHEMA_VERSION,
        manifest_id: instance.instance_id.clone(),
        sha256: instance.sha256.clone(),
        status: "ok".into(),
        original_name: instance.original_name.clone(),
        original_relpath,
        new_filename: Some(final_filename),
        description: Some(validated.description.clone()),
        date: Some(validated.date_iso.clone()),
        date_source: Some("human".into()),
        doc_type: job.doc_type.clone(),
        language: job.language.clone(),
        duplicate_of: duplicate.then(|| instance.sha256.clone()),
        soft_flags,
        flag_reason: None,
        model_versions: job
            .model_versions
            .as_deref()
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or_else(|| serde_json::json!({})),
        processed_at: chrono::Utc::now().to_rfc3339(),
    }
}

fn parse_flags(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|flag| !flag.is_empty())
        .map(str::to_string)
        .collect()
}

fn push_unique(flags: &mut Vec<String>, value: &str) {
    if !flags.iter().any(|flag| flag == value) {
        flags.push(value.to_string());
    }
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FIRST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SECOND: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn flagged_manifest(instance_id: &str, reason: &str, relpath: &str) -> Manifest {
        Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: instance_id.into(),
            sha256: SHA.into(),
            status: "flagged".into(),
            original_name: "same.pdf".into(),
            original_relpath: relpath.into(),
            new_filename: None,
            description: None,
            date: None,
            date_source: None,
            doc_type: None,
            language: None,
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: Some(reason.into()),
            model_versions: serde_json::json!({}),
            processed_at: "2026-07-22T12:00:00Z".into(),
        }
    }

    #[test]
    fn review_query_keeps_duplicate_instances_separate() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("ledger.db");
        let manifests = root.path().join("manifests");
        let ledger = Ledger::open(&db_path).unwrap();
        ledger
            .ingest(SHA, "processing/one/same.pdf", "same.pdf", "pdf")
            .unwrap();
        ledger.set_state(SHA, JobState::Flagged).unwrap();
        for (instance_id, relpath) in [
            (FIRST, "__bl_one/same.pdf"),
            (SECOND, "__bl_two/same.pdf"),
        ] {
            ledger
                .register_instance(
                    instance_id,
                    SHA,
                    &format!("processing/{relpath}"),
                    "same.pdf",
                    "pdf",
                )
                .unwrap();
            ledger
                .set_instance_state(instance_id, InstanceState::Flagged)
                .unwrap();
        }
        write_manifest(
            &manifests,
            &flagged_manifest(FIRST, "FIRST_REASON", "__bl_one/same.pdf"),
        )
        .unwrap();
        write_manifest(
            &manifests,
            &flagged_manifest(SECOND, "SECOND_REASON", "__bl_two/same.pdf"),
        )
        .unwrap();

        let items = list_review_items(&db_path, &manifests, 20).unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|item| {
            item.instance_id == FIRST && item.flag_reason.as_deref() == Some("FIRST_REASON")
        }));
        assert!(items.iter().any(|item| {
            item.instance_id == SECOND && item.flag_reason.as_deref() == Some("SECOND_REASON")
        }));
    }

    #[test]
    fn restore_prefers_the_instance_specific_quarantine_file() {
        let root = tempfile::tempdir().unwrap();
        let processing = root.path().join("processing");
        let quarantine = root.path().join("quarantine");
        std::fs::create_dir_all(&processing).unwrap();
        std::fs::create_dir_all(&quarantine).unwrap();
        let correct = b"correct bytes";
        let digest = hex::encode(Sha256::digest(correct));
        let specific = quarantine.join(format!("{}-same.pdf", &SECOND[..12]));
        let ordinary = quarantine.join("same.pdf");
        std::fs::write(&specific, correct).unwrap();
        std::fs::write(&ordinary, b"other bytes").unwrap();
        let instance = FileInstance {
            instance_id: SECOND.into(),
            sha256: digest,
            original_path: processing
                .join("__bl_two/same.pdf")
                .to_string_lossy()
                .into_owned(),
            original_name: "same.pdf".into(),
            ext: "pdf".into(),
            state: InstanceState::Flagged,
            final_filename: None,
            manifest_id: SECOND.into(),
            created_at: "now".into(),
            updated_at: "now".into(),
        };
        let cfg = Config {
            processing_dir: processing.clone(),
            quarantine_dir: quarantine.clone(),
            ..Config::default()
        };

        let restored = restore_source_for_instance(&cfg, &instance, "__bl_two/same.pdf")
            .unwrap();
        assert_eq!(std::fs::read(restored).unwrap(), correct);
        assert!(!specific.exists());
        assert!(ordinary.exists());
    }
}
