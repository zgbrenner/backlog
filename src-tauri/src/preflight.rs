//! Live runtime readiness checks shared by Settings and pipeline startup.
//!
//! Configuration validation answers whether values are internally coherent.
//! Preflight additionally verifies the current machine: folders are reachable,
//! output locations are writable, binaries and model files exist, and the
//! conversion sidecar can answer a bounded local ping.

use crate::config::Config;
use crate::sidecar::Sidecar;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::Manager;

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
    if let Err(error) = cfg.validate() {
        for config_problem in error.problems {
            problems.push(RuntimeProblem {
                field: config_problem.field,
                code: config_problem.code,
                message: config_problem.message,
                severity: ProblemSeverity::Error,
            });
        }
    }

    let processing_dir_ready = match readable_directory(&cfg.processing_dir) {
        Ok(()) => true,
        Err(message) => {
            push_error(
                &mut problems,
                "processing_dir",
                "processing_unavailable",
                message,
            );
            false
        }
    };
    let outbox_writable = match writable_directory(&cfg.manifests_dir()) {
        Ok(()) => true,
        Err(message) => {
            push_error(
                &mut problems,
                "outbox_dir",
                "outbox_not_writable",
                message,
            );
            false
        }
    };
    let quarantine_writable = match writable_directory(&cfg.quarantine_dir) {
        Ok(()) => true,
        Err(message) => {
            push_error(
                &mut problems,
                "quarantine_dir",
                "quarantine_not_writable",
                message,
            );
            false
        }
    };
    let cache_writable = match writable_directory(&cfg.cache_dir) {
        Ok(()) => true,
        Err(message) => {
            push_error(
                &mut problems,
                "cache_dir",
                "cache_not_writable",
                message,
            );
            false
        }
    };

    let primary_model_found = cfg.slm_primary_gguf.is_file();
    let escalation_model_found = cfg.slm_escalation_gguf.is_file();
    let sidecar_path = resolve_binary(app, "convertd");
    let llama_server_path = resolve_binary(app, "llama-server");
    let grammar_path = resolve_resource(app, "name.gbnf");
    let sidecar_found = sidecar_path.is_some();
    let llama_server_found = llama_server_path.is_some();
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

    let sidecar_ok = if let Some(executable) = sidecar_path {
        let timeout = Duration::from_secs(cfg.sidecar_timeout_secs.clamp(1, 5));
        match tokio::task::spawn_blocking(move || {
            let sidecar = Sidecar::with_timeout(executable, timeout);
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

pub fn resolve_resource(app: &tauri::AppHandle, relative: &str) -> PathBuf {
    app.path()
        .resolve(
            format!("resources/{relative}"),
            tauri::path::BaseDirectory::Resource,
        )
        .unwrap_or_else(|_| PathBuf::from(format!("resources/{relative}")))
}

pub fn resolve_binary(app: &tauri::AppHandle, name: &str) -> Option<PathBuf> {
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let mut candidates = Vec::new();
    if let Some(directory) = executable_dir {
        candidates.push(directory.join(name));
        candidates.push(directory.join(format!("{name}.exe")));
    }
    candidates.push(resolve_resource(app, name));
    candidates.push(resolve_resource(app, &format!("{name}.exe")));

    for candidate in candidates {
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    resolve_on_path(name)
}

fn resolve_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let mut names = vec![name.to_string()];
    #[cfg(windows)]
    {
        if !name.to_ascii_lowercase().ends_with(".exe") {
            names.push(format!("{name}.exe"));
        }
    }
    for directory in std::env::split_paths(&path) {
        for filename in &names {
            let candidate = directory.join(filename);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
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
    let probe = path.join(format!(
        ".backlog-preflight-{}-{nonce}.tmp",
        std::process::id()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
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

fn push_error(
    problems: &mut Vec<RuntimeProblem>,
    field: &str,
    code: &str,
    message: impl Into<String>,
) {
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
    fn unchecked_status_is_not_startable() {
        let status = RuntimeStatus::unchecked(false, false);
        assert!(!status.configured);
        assert!(!status.checked);
        assert_eq!(status.problems[0].code, "preflight_required");
    }
}
