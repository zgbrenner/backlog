//! BackLog configuration. Loaded from `backlog.config.json` next to the app
//! data dir; every field has a sane default so first launch works with only
//! the folder paths filled in from the UI.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// OneDrive-synced folder Power Automate Flow 1 moves intake files into.
    pub processing_dir: PathBuf,
    /// OneDrive-synced folder the app writes per-file manifests into
    /// (Flow 2 triggers on `<outbox_dir>/_manifests`).
    pub outbox_dir: PathBuf,
    /// Local quarantine for flagged files (not synced).
    pub quarantine_dir: PathBuf,
    /// Local cache: converted markdown + evidence bundles, keyed by sha256.
    pub cache_dir: PathBuf,

    /// llama-server settings.
    pub llama_port: u16,
    pub slm_primary_gguf: PathBuf,
    pub slm_escalation_gguf: PathBuf,
    pub slm_parallel: u8,
    /// Max evidence tokens (approximate, chars/4) sent to the SLM.
    pub evidence_token_budget: usize,

    /// Optional fine-tuned Ettin token classifier directory (HF format).
    /// Empty string disables the Ettin lane gracefully.
    pub ettin_model_dir: String,

    /// Worker pool sizes.
    pub convert_workers: usize,

    /// Pace manifest emission (per minute, 0 = unlimited) to stay under
    /// Power Automate connector throttling on huge batches.
    pub manifest_emit_per_min: u32,

    /// Pages sampled for oversized documents.
    pub max_head_pages: usize,
    pub max_tail_pages: usize,

    /// Filename policy.
    pub max_filename_len: usize,

    /// Retry policy.
    pub max_stage_attempts: u8,
    pub per_file_wall_clock_secs: u64,

    /// Keep converted markdown in the cache after a file is successfully
    /// emitted. Default false: the raw document text is purged on emit so the
    /// cache never accumulates document bodies (flagged files awaiting review
    /// keep their cache until resolved). Set true only to deliberately build
    /// an Ettin training corpus — an explicit, auditable opt-in.
    pub retain_cache: bool,
    /// Days after which an orphaned cache entry is swept on startup.
    pub cache_ttl_days: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            processing_dir: PathBuf::new(),
            outbox_dir: PathBuf::new(),
            quarantine_dir: PathBuf::new(),
            cache_dir: PathBuf::new(),
            llama_port: 8137,
            slm_primary_gguf: PathBuf::from("models/LFM2.5-350M-Q8_0.gguf"),
            slm_escalation_gguf: PathBuf::from("models/LFM2.5-1.2B-Instruct-Q4_K_M.gguf"),
            slm_parallel: 4,
            evidence_token_budget: 1500,
            ettin_model_dir: String::new(),
            convert_workers: default_convert_workers(),
            manifest_emit_per_min: 0,
            max_head_pages: 10,
            max_tail_pages: 3,
            max_filename_len: 120,
            max_stage_attempts: 3,
            per_file_wall_clock_secs: 90,
            retain_cache: false,
            cache_ttl_days: 7,
        }
    }
}

fn lexical_norm(p: &Path) -> PathBuf {
    // Lexical normalization only — folders may not exist yet, so canonicalize
    // isn't available. Good enough to catch equal/nested paths.
    p.components().collect()
}

fn default_convert_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).clamp(1, 6))
        .unwrap_or(2)
}

impl Config {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                log::warn!("config parse failed ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.outbox_dir.join("_manifests")
    }

    pub fn ready(&self) -> bool {
        !self.processing_dir.as_os_str().is_empty()
            && !self.outbox_dir.as_os_str().is_empty()
            && !self.quarantine_dir.as_os_str().is_empty()
    }

    /// Reject configurations that would corrupt processing: unset folders,
    /// duplicate folders, or folders nested inside one another. The watcher is
    /// recursive over the processing dir, so a nested outbox/cache/quarantine
    /// would feed the app's own manifests and cached markdown back into the
    /// pipeline as if they were intake documents.
    pub fn validate(&self) -> Result<(), String> {
        if !self.ready() {
            return Err("Set the Processing, Outbox, and Quarantine folders first.".into());
        }
        let named: [(&str, &Path); 4] = [
            ("Processing", self.processing_dir.as_path()),
            ("Outbox", self.outbox_dir.as_path()),
            ("Quarantine", self.quarantine_dir.as_path()),
            ("Cache", self.cache_dir.as_path()),
        ];
        for i in 0..named.len() {
            let (a_name, a_path) = named[i];
            if a_path.as_os_str().is_empty() {
                continue;
            }
            let a = lexical_norm(a_path);
            for (b_name, b_path) in named.iter().skip(i + 1) {
                if b_path.as_os_str().is_empty() {
                    continue;
                }
                let b = lexical_norm(b_path);
                if a == b {
                    return Err(format!("{a_name} and {b_name} folders must be different."));
                }
                if a.starts_with(&b) || b.starts_with(&a) {
                    return Err(format!(
                        "{a_name} and {b_name} folders must not be nested inside each other."
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(proc: &str, out: &str, quar: &str, cache: &str) -> Config {
        Config {
            processing_dir: proc.into(),
            outbox_dir: out.into(),
            quarantine_dir: quar.into(),
            cache_dir: cache.into(),
            ..Default::default()
        }
    }

    #[test]
    fn accepts_distinct_folders() {
        assert!(cfg("/a/proc", "/a/out", "/a/quar", "/a/cache").validate().is_ok());
    }

    #[test]
    fn rejects_nested_outbox_under_processing() {
        let c = cfg("/a/proc", "/a/proc/out", "/a/quar", "/a/cache");
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_folders() {
        let c = cfg("/a/proc", "/a/proc", "/a/quar", "/a/cache");
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_unset_folders() {
        assert!(cfg("", "", "", "").validate().is_err());
    }
}
