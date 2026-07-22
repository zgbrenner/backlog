//! §8 handoff: one JSON manifest per file into <outbox>/_manifests, which
//! Power Automate Flow 2 triggers on. Writes are atomic (tmp + rename) so
//! OneDrive never syncs a half-written manifest, and emission can be paced
//! to stay under PA connector throttling.

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub sha256: String,
    pub status: String, // "ok" | "flagged"
    pub original_name: String,
    pub original_relpath: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_filename: Option<String>, // "YYYY-MM-DD Subject.ext"
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

pub fn write_manifest(dir: &Path, m: &Manifest) -> anyhow::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let final_path = dir.join(format!("{}.json", m.sha256));
    let tmp_path = dir.join(format!(".{}.json.tmp", m.sha256));
    std::fs::write(&tmp_path, serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

/// Token-bucket pacer for manifest emission (0 = unlimited).
pub struct Pacer {
    per_min: u32,
    last_emit: std::sync::Mutex<Vec<std::time::Instant>>,
}

impl Pacer {
    pub fn new(per_min: u32) -> Self {
        Self { per_min, last_emit: std::sync::Mutex::new(Vec::new()) }
    }

    pub async fn permit(&self) {
        if self.per_min == 0 {
            return;
        }
        loop {
            let wait = {
                let mut v = self.last_emit.lock().unwrap();
                let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(60);
                v.retain(|t| *t > cutoff);
                if (v.len() as u32) < self.per_min {
                    v.push(std::time::Instant::now());
                    None
                } else {
                    Some(std::time::Duration::from_millis(1500))
                }
            };
            match wait {
                None => return,
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }
}
