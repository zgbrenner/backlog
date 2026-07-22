//! BackLog configuration. Loaded from `backlog.config.json` next to the app
//! data dir. Validation is centralized here so settings, preflight, and Start
//! all enforce the same runtime requirements.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path, PathBuf};

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

    /// Maximum wait for one convertd request. A timed-out process is killed and
    /// lazily respawned on the next request.
    pub sidecar_timeout_secs: u64,

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigProblem {
    pub field: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigError {
    pub problems: Vec<ConfigProblem>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid configuration")?;
        for problem in &self.problems {
            write!(formatter, "; {}: {}", problem.field, problem.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigError {}

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
            sidecar_timeout_secs: 45,
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
        .map(|count| count.get().saturating_sub(2).clamp(1, 6))
        .unwrap_or(2)
}

impl Config {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|error| {
                log::warn!("config parse failed ({error}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigSaveError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ConfigSaveError::Io)?;
        }
        let contents = serde_json::to_string_pretty(self).map_err(ConfigSaveError::Serialize)?;
        std::fs::write(path, contents).map_err(ConfigSaveError::Io)
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.outbox_dir.join("_manifests")
    }

    pub fn ready(&self) -> bool {
        self.validate().is_ok()
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut problems = Vec::new();

        require_path(&mut problems, "processing_dir", &self.processing_dir);
        require_path(&mut problems, "outbox_dir", &self.outbox_dir);
        require_path(&mut problems, "quarantine_dir", &self.quarantine_dir);
        require_path(&mut problems, "cache_dir", &self.cache_dir);

        let roots = [
            ("processing_dir", &self.processing_dir),
            ("outbox_dir", &self.outbox_dir),
            ("quarantine_dir", &self.quarantine_dir),
            ("cache_dir", &self.cache_dir),
        ];
        for (index, (left_name, left_path)) in roots.iter().enumerate() {
            if left_path.as_os_str().is_empty() {
                continue;
            }
            for (right_name, right_path) in roots.iter().skip(index + 1) {
                if right_path.as_os_str().is_empty() {
                    continue;
                }
                if paths_overlap(left_path, right_path) {
                    problem(
                        &mut problems,
                        left_name,
                        "overlapping_path",
                        format!(
                            "must not be the same as, contain, or sit inside {right_name}"
                        ),
                    );
                }
            }
        }

        require_model(
            &mut problems,
            "slm_primary_gguf",
            &self.slm_primary_gguf,
        );
        require_model(
            &mut problems,
            "slm_escalation_gguf",
            &self.slm_escalation_gguf,
        );

        if !(1024..=65534).contains(&self.llama_port) {
            problem(
                &mut problems,
                "llama_port",
                "invalid_port",
                "must be between 1024 and 65534 so the escalation port is also valid",
            );
        }
        if self.slm_parallel == 0 || self.slm_parallel > 32 {
            problem(
                &mut problems,
                "slm_parallel",
                "invalid_worker_count",
                "must be between 1 and 32",
            );
        }
        if self.convert_workers == 0 || self.convert_workers > 64 {
            problem(
                &mut problems,
                "convert_workers",
                "invalid_worker_count",
                "must be between 1 and 64",
            );
        }
        if self.evidence_token_budget == 0 {
            problem(
                &mut problems,
                "evidence_token_budget",
                "invalid_budget",
                "must be greater than zero",
            );
        }
        if self.sidecar_timeout_secs == 0
            || self.sidecar_timeout_secs > self.per_file_wall_clock_secs
        {
            problem(
                &mut problems,
                "sidecar_timeout_secs",
                "invalid_timeout",
                "must be greater than zero and no longer than per_file_wall_clock_secs",
            );
        }
        if !(5..=3600).contains(&self.per_file_wall_clock_secs) {
            problem(
                &mut problems,
                "per_file_wall_clock_secs",
                "invalid_timeout",
                "must be between 5 and 3600 seconds",
            );
        }
        if self.max_stage_attempts == 0 || self.max_stage_attempts > 10 {
            problem(
                &mut problems,
                "max_stage_attempts",
                "invalid_retry_count",
                "must be between 1 and 10",
            );
        }
        if self.max_head_pages == 0 || self.max_head_pages > 100 {
            problem(
                &mut problems,
                "max_head_pages",
                "invalid_page_count",
                "must be between 1 and 100",
            );
        }
        if self.max_tail_pages > 100 {
            problem(
                &mut problems,
                "max_tail_pages",
                "invalid_page_count",
                "must be no greater than 100",
            );
        }
        if !(32..=255).contains(&self.max_filename_len) {
            problem(
                &mut problems,
                "max_filename_len",
                "invalid_filename_limit",
                "must be between 32 and 255 characters",
            );
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(ConfigError { problems })
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigSaveError {
    #[error(transparent)]
    Validation(#[from] ConfigError),
    #[error("failed to serialize configuration: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to write configuration: {0}")]
    Io(#[source] std::io::Error),
}

fn require_path(problems: &mut Vec<ConfigProblem>, field: &str, path: &Path) {
    if path.as_os_str().is_empty() {
        problem(
            problems,
            field,
            "required_path",
            "select a non-empty folder",
        );
    }
}

fn require_model(problems: &mut Vec<ConfigProblem>, field: &str, path: &Path) {
    if path.as_os_str().is_empty() {
        problem(
            problems,
            field,
            "required_model",
            "select a GGUF model file",
        );
    } else if !path.is_file() {
        problem(
            problems,
            field,
            "model_not_found",
            format!("model file does not exist: {}", path.display()),
        );
    }
}

fn problem(
    problems: &mut Vec<ConfigProblem>,
    field: &str,
    code: &str,
    message: impl Into<String>,
) {
    problems.push(ConfigProblem {
        field: field.into(),
        code: code.into(),
        message: message.into(),
    });
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = normalized_components(left);
    let right = normalized_components(right);
    is_component_prefix(&left, &right) || is_component_prefix(&right, &left)
}

fn is_component_prefix(left: &[String], right: &[String]) -> bool {
    left.len() <= right.len() && left.iter().zip(right).all(|(a, b)| a == b)
}

fn normalized_components(path: &Path) -> Vec<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let resolved = absolute.canonicalize().unwrap_or(absolute);
    let mut components = Vec::new();
    for component in resolved.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            other => components.push(other.as_os_str().to_string_lossy().to_lowercase()),
        }
    }
    components
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config(root: &Path) -> Config {
        let processing = root.join("processing");
        let outbox = root.join("outbox");
        let quarantine = root.join("quarantine");
        let cache = root.join("cache");
        for path in [&processing, &outbox, &quarantine, &cache] {
            std::fs::create_dir_all(path).unwrap();
        }
        let primary = root.join("primary.gguf");
        let escalation = root.join("escalation.gguf");
        std::fs::write(&primary, b"fixture").unwrap();
        std::fs::write(&escalation, b"fixture").unwrap();

        Config {
            processing_dir: processing,
            outbox_dir: outbox,
            quarantine_dir: quarantine,
            cache_dir: cache,
            slm_primary_gguf: primary,
            slm_escalation_gguf: escalation,
            ..Config::default()
        }
    }

    #[test]
    fn valid_configuration_passes() {
        let dir = tempfile::tempdir().unwrap();
        valid_config(dir.path()).validate().unwrap();
    }

    #[test]
    fn overlapping_roots_and_zero_workers_are_reported_together() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = valid_config(dir.path());
        config.quarantine_dir = config.processing_dir.join("quarantine");
        config.convert_workers = 0;
        config.slm_parallel = 0;

        let error = config.validate().unwrap_err();
        let codes: Vec<&str> = error
            .problems
            .iter()
            .map(|problem| problem.code.as_str())
            .collect();
        assert!(codes.contains(&"overlapping_path"));
        assert_eq!(
            codes
                .iter()
                .filter(|code| **code == "invalid_worker_count")
                .count(),
            2
        );
    }

    #[test]
    fn missing_models_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = valid_config(dir.path());
        config.slm_primary_gguf = dir.path().join("missing.gguf");

        let error = config.validate().unwrap_err();
        assert!(error.problems.iter().any(|problem| {
            problem.field == "slm_primary_gguf" && problem.code == "model_not_found"
        }));
    }

    #[test]
    fn save_does_not_persist_invalid_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = valid_config(dir.path());
        config.convert_workers = 0;
        let destination = dir.path().join("config.json");

        assert!(matches!(
            config.save(&destination),
            Err(ConfigSaveError::Validation(_))
        ));
        assert!(!destination.exists());
    }
}
