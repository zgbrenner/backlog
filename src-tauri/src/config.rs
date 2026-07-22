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
        }
    }
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
        self.processing_dir.as_os_str().len() > 0
            && self.outbox_dir.as_os_str().len() > 0
            && self.quarantine_dir.as_os_str().len() > 0
    }
}
