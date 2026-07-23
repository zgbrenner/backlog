//! Live runtime readiness checks shared by Settings and pipeline startup.
//!
//! Configuration validation (`Config::validate`) only answers whether the
//! configured values are internally coherent (folders set, not nested, etc).
//! Preflight goes further and verifies the current machine: folders are
//! reachable, output locations are actually writable, binaries and model
//! files exist on disk, and the conversion sidecar can answer a bounded
//! local ping. `configured` is true only when every check passes — fail
//! closed by default so a half-set-up machine can't silently start a
//! pipeline that will fail on the first file.

use crate::config::Config;
use crate::sidecar::Sidecar;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProblem {
    pub field: String,
    pub code: String,
    pub message: String,
    pub severity: ProblemSeverity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub configured: bool,
    pub checked: bool,
    pub running: bool,
    pub paused: bool,
    pub processing_dir_ready: bool,
    pub outbox_writable: bool,
    pub quarantine_writable: bool,
    pub cache_writable: bool,
    pub sidecar_found: bool,
    pub sidecar_ok: bool,
    pub llama_server_found: bool,
    pub grammar_found: bool,
    pub primary_model_found: bool,
    pub escalation_model_found: bool,
    pub offline_runtime: bool,
    pub checked_at: Option<String>,
    pub problems: Vec<RuntimeProblem>,
}

impl RuntimeStatus {
    /// Fail-closed default for "no preflight has run yet" (fresh launch, or
    /// settings just changed). Never `configured`, so the UI keeps Start
    /// disabled until a real check passes.
    pub fn unchecked(running: bool, paused: bool) -> Self {
        Self {
            configured: false,
            checked: false,
            running,
            paused,
            processing_dir_ready: false,
            outbox_writable: false,
            quarantine_writable: false,
            cache_writable: false,
            sidecar_found: false,
            sidecar_ok: false,
            llama_server_found: false,
            grammar_found: false,
            primary_model_found: false,
            escalation_model_found: false,
            offline_runtime: true,
            checked_at: None,
            problems: vec![RuntimeProblem {
                field: "preflight".into(),
                code: "preflight_required".into(),
                message: "Run preflight after saving settings before starting BackLog.".into(),
                severity: ProblemSeverity::Error,
            }],
        }
    }

    pub fn with_runtime(mut self, running: bool, paused: bool) -> Self {
        self.running = running;
        self.paused = running && paused;
        self
    }

    pub fn summary(&self) -> String {
        let messages: Vec<&str> = self
            .problems
            .iter()
            .filter(|problem| problem.severity == ProblemSeverity::Error)
            .map(|problem| problem.message.as_str())
            .collect();
        if messages.is_empty() {
            "runtime preflight did not pass".into()
        } else {
            messages.join("; ")
        }
    }
}

pub async fn run(app: &tauri::AppHandle, cfg: &Config, running: bool, paused: bool) -> RuntimeStatus {
    let mut problems = Vec::new();

    // THIS branch's `Config::validate` returns a single human-readable
    // message rather than PRIOR's per-field `ConfigError`; fold it into one
    // problem entry instead of iterating structured sub-problems.
    if let Err(message) = cfg.validate() {
        push_error(&mut problems, "config", "config_invalid", message);
    }

    let processing_dir_ready = match readable_directory(&cfg.processing_dir) {
        Ok(()) => true,
        Err(message) => {
            push_error(&mut problems, "processing_dir", "processing_unavailable", message);
            false
        }
    };
    let outbox_writable = check_writable_root(
        &mut problems,
        "outbox_dir",
        "outbox_not_writable",
        &cfg.outbox_dir,
        &cfg.manifests_dir(),
    );
    let quarantine_writable = check_writable_root(
        &mut problems,
        "quarantine_dir",
        "quarantine_not_writable",
        &cfg.quarantine_dir,
        &cfg.quarantine_dir,
    );
    let cache_writable = check_writable_root(
        &mut problems,
        "cache_dir",
        "cache_not_writable",
        &cfg.cache_dir,
        &cfg.cache_dir,
    );

    let primary_model_found = cfg.slm_primary_gguf.is_file();
    let escalation_model_found = cfg.slm_escalation_gguf.is_file();

    let sidecar_path = crate::binary(app, "convertd");
    let llama_server_path = crate::binary(app, "llama-server");
    let grammar_path = crate::resource(app, "name.gbnf");

    let sidecar_found = binary_exists(&sidecar_path);
    let llama_server_found = binary_exists(&llama_server_path);
    let grammar_found = grammar_path.is_file();

    if !sidecar_found {
        push_error(
            &mut problems,
            "sidecar",
            "sidecar_not_found",
            "The convertd sidecar binary was not found beside the app or on PATH.",
        );
    }
    if !llama_server_found {
        push_error(
            &mut problems,
            "llama_server",
            "llama_server_not_found",
            "The llama-server binary was not found beside the app or on PATH.",
        );
    }
    if !grammar_found {
        push_error(
            &mut problems,
            "grammar",
            "grammar_not_found",
            format!("The naming grammar was not found at {}.", grammar_path.display()),
        );
    }

    let sidecar_ok = if sidecar_found {
        // Bounded and short: this is a liveness probe, not the real
        // per-request timeout used once the pipeline is running, so clamp
        // well below `cfg.sidecar_timeout_secs` regardless of how that's
        // configured.
        let timeout = Duration::from_secs(cfg.sidecar_timeout_secs.clamp(1, 5));
        let executable = sidecar_path.clone();
        let models_dir = crate::model_download::resolve_models_dir(app);
        match tokio::task::spawn_blocking(move || {
            let sidecar = Sidecar::with_timeout(executable, timeout).with_models_dir(models_dir);
            sidecar.ping()
        })
        .await
        {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                push_error(
                    &mut problems,
                    "sidecar",
                    "sidecar_ping_failed",
                    format!("The local conversion sidecar did not answer ping: {error}"),
                );
                false
            }
            Err(error) => {
                push_error(
                    &mut problems,
                    "sidecar",
                    "sidecar_check_failed",
                    format!("The sidecar readiness check could not complete: {error}"),
                );
                false
            }
        }
    } else {
        false
    };

    let configured = problems
        .iter()
        .all(|problem| problem.severity != ProblemSeverity::Error)
        && processing_dir_ready
        && outbox_writable
        && quarantine_writable
        && cache_writable
        && sidecar_found
        && sidecar_ok
        && llama_server_found
        && grammar_found
        && primary_model_found
        && escalation_model_found;

    RuntimeStatus {
        configured,
        checked: true,
        running,
        paused: running && paused,
        processing_dir_ready,
        outbox_writable,
        quarantine_writable,
        cache_writable,
        sidecar_found,
        sidecar_ok,
        llama_server_found,
        grammar_found,
        primary_model_found,
        escalation_model_found,
        offline_runtime: true,
        checked_at: Some(chrono::Utc::now().to_rfc3339()),
        problems,
    }
}

/// `crate::binary` falls back to a bare executable name (e.g. `"convertd"`)
/// for the OS to resolve against PATH when the process is actually spawned;
/// that fallback carries no filesystem guarantee on its own. Preflight needs
/// a truthful "found" signal ahead of spawning, so a bare-name result is
/// verified against PATH here instead of assumed.
fn binary_exists(path: &Path) -> bool {
    if path.is_file() {
        return true;
    }
    let is_bare_name = path
        .parent()
        .map(|parent| parent.as_os_str().is_empty())
        .unwrap_or(true);
    if !is_bare_name {
        return false;
    }
    match path.to_str() {
        Some(name) => found_on_path(name),
        None => false,
    }
}

fn found_on_path(name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    let mut names = vec![name.to_string()];
    #[cfg(windows)]
    {
        if !name.to_ascii_lowercase().ends_with(".exe") {
            names.push(format!("{name}.exe"));
        }
    }
    std::env::split_paths(&path_var).any(|dir| names.iter().any(|filename| dir.join(filename).is_file()))
}

fn readable_directory(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("Select the Processing folder.".into());
    }
    if !path.is_dir() {
        return Err(format!("Folder does not exist: {}", path.display()));
    }
    std::fs::read_dir(path)
        .map(|_| ())
        .map_err(|error| format!("Folder cannot be read ({}): {error}", path.display()))
}

fn check_writable_root(
    problems: &mut Vec<RuntimeProblem>,
    field: &str,
    code: &str,
    configured_root: &Path,
    probe_path: &Path,
) -> bool {
    if configured_root.as_os_str().is_empty() {
        return false;
    }
    match writable_directory(probe_path) {
        Ok(()) => true,
        Err(message) => {
            push_error(problems, field, code, message);
            false
        }
    }
}

fn writable_directory(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("Select a non-empty folder.".into());
    }
    std::fs::create_dir_all(path)
        .map_err(|error| format!("Folder cannot be created ({}): {error}", path.display()))?;
    if !path.is_dir() {
        return Err(format!("Path is not a folder: {}", path.display()));
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let probe = path.join(format!(".backlog-preflight-{}-{nonce}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&probe)?;
        file.write_all(b"backlog-preflight")?;
        file.sync_all()?;
        drop(file);
        std::fs::remove_file(&probe)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&probe);
    }
    result.map_err(|error| format!("Folder is not writable ({}): {error}", path.display()))
}

fn push_error(problems: &mut Vec<RuntimeProblem>, field: &str, code: &str, message: impl Into<String>) {
    problems.push(RuntimeProblem {
        field: field.into(),
        code: code.into(),
        message: message.into(),
        severity: ProblemSeverity::Error,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_probe_creates_missing_directory_and_cleans_up() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("nested").join("outbox");
        writable_directory(&target).unwrap();
        assert!(target.is_dir());
        assert_eq!(std::fs::read_dir(target).unwrap().count(), 0);
    }

    #[test]
    fn writable_probe_rejects_a_file_path() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("not-a-directory");
        std::fs::write(&target, b"fixture").unwrap();
        assert!(writable_directory(&target).is_err());
    }

    #[test]
    fn blank_root_does_not_create_a_relative_probe_directory() {
        let root = tempfile::tempdir().unwrap();
        let relative_probe = root.path().join("_manifests");
        let mut problems = Vec::new();
        assert!(!check_writable_root(
            &mut problems,
            "outbox_dir",
            "outbox_not_writable",
            Path::new(""),
            &relative_probe,
        ));
        assert!(!relative_probe.exists());
    }

    #[test]
    fn unchecked_status_is_not_startable() {
        let status = RuntimeStatus::unchecked(false, false);
        assert!(!status.configured);
        assert!(!status.checked);
        assert_eq!(status.problems[0].code, "preflight_required");
    }
}
