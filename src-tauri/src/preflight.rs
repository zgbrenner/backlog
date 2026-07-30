//! Live runtime readiness checks shared by Settings and pipeline startup.
//!
//! Configuration validation (`Config::validate`) only answers whether the
//! configured values are internally coherent (folders set, not nested, etc).
//! Preflight goes further and verifies the current machine: folders are
//! reachable, output locations are actually writable, binaries and model
//! files exist on disk *beside the app*, and both sidecars actually launch.
//! `configured` is true only when every check passes — fail closed by default
//! so a half-set-up machine can't silently start a pipeline that will fail on
//! the first file.
//!
//! Every failure carries a message written for someone who has never opened a
//! terminal, the technical form in `detail`, and — where the app can fix it
//! itself — an `action` the UI turns into a button.

use crate::config::Config;
use crate::sidecar::Sidecar;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many entries of the Processing folder to enumerate for the "is this
/// really the folder you meant?" readout. A backfill folder can hold tens of
/// thousands of files and preflight runs on every Settings save.
const PROCESSING_SCAN_CAP: u64 = 5_000;
const PROCESSING_SAMPLE_LEN: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemSeverity {
    Error,
    Warning,
}

/// Something the app can do about a problem on the user's behalf. The webview
/// renders these as the one button that fixes the row it sits on, instead of
/// leaving a non-technical user to work out that "Folder does not exist" is
/// repairable and "binary not found" is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemAction {
    CreateFolder,
    DownloadModels,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProblem {
    pub field: String,
    pub code: String,
    /// Plain-language, addressed to the office worker who runs this appliance.
    pub message: String,
    /// The paths / process names an IT contact needs, kept out of `message`
    /// so the primary sentence stays readable.
    pub detail: Option<String>,
    pub severity: ProblemSeverity,
    pub action: Option<ProblemAction>,
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
    pub llama_server_ok: bool,
    pub grammar_found: bool,
    pub primary_model_found: bool,
    pub escalation_model_found: bool,
    pub offline_runtime: bool,
    /// Number of entries in the Processing folder, capped at
    /// `PROCESSING_SCAN_CAP`, plus the first few names — the only way a user
    /// can confirm from inside the app that they pointed it at the folder
    /// they meant.
    pub processing_entry_count: Option<u64>,
    pub processing_entry_count_capped: bool,
    pub processing_sample: Vec<String>,
    pub checked_at: Option<String>,
    pub problems: Vec<RuntimeProblem>,
}

impl RuntimeStatus {
    /// Every boolean check in display order, paired with the plain-language
    /// name used when something has to name it in a sentence.
    fn checks(&self) -> [(&'static str, bool); 11] {
        [
            ("the Processing folder", self.processing_dir_ready),
            ("the Outbox folder", self.outbox_writable),
            ("the Quarantine folder", self.quarantine_writable),
            ("the working folder", self.cache_writable),
            ("the document converter", self.sidecar_found),
            ("the document converter's response", self.sidecar_ok),
            ("the naming engine", self.llama_server_found),
            ("the naming engine's start-up", self.llama_server_ok),
            ("the naming rules file", self.grammar_found),
            ("the everyday model file", self.primary_model_found),
            ("the backup model file", self.escalation_model_found),
        ]
    }

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
            llama_server_ok: false,
            grammar_found: false,
            primary_model_found: false,
            escalation_model_found: false,
            offline_runtime: true,
            processing_entry_count: None,
            processing_entry_count_capped: false,
            processing_sample: Vec::new(),
            checked_at: None,
            problems: vec![RuntimeProblem {
                field: "preflight".into(),
                code: "preflight_required".into(),
                message: "Press Check again to test this computer before starting BackLog.".into(),
                detail: None,
                severity: ProblemSeverity::Error,
                action: None,
            }],
        }
    }

    pub fn with_runtime(mut self, running: bool, paused: bool) -> Self {
        self.running = running;
        self.paused = running && paused;
        self
    }

    /// Names of the checks that are currently failing, in display order.
    pub fn failed_checks(&self) -> Vec<&'static str> {
        self.checks()
            .into_iter()
            .filter(|(_, ok)| !ok)
            .map(|(label, _)| label)
            .collect()
    }

    /// One sentence explaining why Start is refusing. Never the bare
    /// "runtime preflight did not pass" it used to return: when no problem
    /// carries a message, name the checks that are false, because that string
    /// is what a user sees in a toast after clicking a button they were
    /// invited to click.
    pub fn summary(&self) -> String {
        let messages: Vec<&str> = self
            .problems
            .iter()
            .filter(|problem| problem.severity == ProblemSeverity::Error)
            .map(|problem| problem.message.as_str())
            .collect();
        if !messages.is_empty() {
            return messages.join(" ");
        }
        let failed = self.failed_checks();
        if failed.is_empty() {
            "BackLog has not checked this computer yet. Open Settings and press Check.".into()
        } else {
            format!(
                "BackLog is not ready yet: {} could not be verified.",
                join_and(&failed)
            )
        }
    }
}

fn join_and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// Everything preflight needs from the Tauri app handle, resolved up front.
/// Splitting this out is what lets `run_with` — and therefore the whole
/// readiness policy — be exercised by unit tests, which cannot build an
/// `AppHandle`.
pub struct RuntimePaths {
    pub sidecar: PathBuf,
    pub llama_server: PathBuf,
    pub grammar: PathBuf,
    pub models_dir: PathBuf,
    /// Set when the app cannot tell where it is installed, which is the one
    /// case where there is no sidecar path to report at all. Empty paths are
    /// then carried deliberately: `binary_exists` refuses them, so nothing is
    /// spawned and no bare name reaches `%PATH%`.
    pub install_dir_error: Option<String>,
}

impl RuntimePaths {
    fn from_app(app: &tauri::AppHandle) -> Self {
        let sidecar = crate::binary(app, "convertd");
        let llama_server = crate::binary(app, "llama-server");
        let install_dir_error = sidecar
            .as_ref()
            .err()
            .or(llama_server.as_ref().err())
            .cloned();
        Self {
            sidecar: sidecar.unwrap_or_default(),
            llama_server: llama_server.unwrap_or_default(),
            grammar: crate::resource(app, "name.gbnf"),
            models_dir: crate::model_download::resolve_models_dir(app),
            install_dir_error,
        }
    }
}

pub async fn run(
    app: &tauri::AppHandle,
    cfg: &Config,
    running: bool,
    paused: bool,
) -> RuntimeStatus {
    run_with(&RuntimePaths::from_app(app), cfg, running, paused).await
}

pub async fn run_with(
    paths: &RuntimePaths,
    cfg: &Config,
    running: bool,
    paused: bool,
) -> RuntimeStatus {
    let mut problems = Vec::new();

    // THIS branch's `Config::validate` returns a single human-readable
    // message rather than PRIOR's per-field `ConfigError`; fold it into one
    // problem entry instead of iterating structured sub-problems.
    if let Err(message) = cfg.validate() {
        push_error(&mut problems, "config", "config_invalid", message);
    }

    let processing = check_processing_dir(&mut problems, &cfg.processing_dir);
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
    if !primary_model_found {
        // The primary model gates `configured`. A fresh install with no model
        // used to show a green "All checks passed" box above a disabled Start
        // button, so the blocking state gets a named problem and a button.
        problems.push(RuntimeProblem {
            field: "models".into(),
            code: "models_missing".into(),
            message: "BackLog still needs to download the two model files it uses to name \
                      documents. Press Download models below; it is a one-time download of \
                      about 2.5 GB."
                .into(),
            detail: Some(format!(
                "expected {} and {}",
                cfg.slm_primary_gguf.display(),
                cfg.slm_escalation_gguf.display()
            )),
            severity: ProblemSeverity::Error,
            action: Some(ProblemAction::DownloadModels),
        });
    } else if cfg.using_primary_for_escalation() {
        problems.push(RuntimeProblem {
            field: "slm_escalation_gguf".into(),
            code: "escalation_model_missing_using_primary".into(),
            message: "The optional backup model is not installed. BackLog is ready to work \
                      using the everyday model for backup naming attempts."
                .into(),
            detail: Some(format!(
                "primary model will safely handle escalation attempts until {} is installed; \
                 using {}.",
                cfg.slm_escalation_gguf.display(),
                cfg.effective_escalation_gguf().display()
            )),
            severity: ProblemSeverity::Warning,
            action: Some(ProblemAction::DownloadModels),
        });
    }

    let sidecar_found = binary_exists(&paths.sidecar);
    let llama_server_found = binary_exists(&paths.llama_server);
    let grammar_found = paths.grammar.is_file();

    // One honest cause beats two misleading symptoms: with no install
    // location there is no path that "cannot be found", and telling the user
    // to look for a file BackLog cannot name would waste their support call.
    if let Some(detail) = &paths.install_dir_error {
        push_error(
            &mut problems,
            "install_dir",
            "install_dir_unknown",
            "BackLog could not work out where it is installed, so it cannot start the parts of \
             itself that read documents and suggest names. Reinstalling BackLog fixes this."
                .to_string(),
        );
        if let Some(problem) = problems.last_mut() {
            problem.detail = Some(detail.clone());
        }
    } else {
        if !sidecar_found {
            push_missing_component(
                &mut problems,
                "sidecar",
                "sidecar_not_found",
                "BackLog cannot find the part of itself that reads your documents.",
                &paths.sidecar,
            );
        }
        if !llama_server_found {
            push_missing_component(
                &mut problems,
                "llama_server",
                "llama_server_not_found",
                "BackLog cannot find the part of itself that suggests file names.",
                &paths.llama_server,
            );
        }
    }
    if !grammar_found {
        push_missing_component(
            &mut problems,
            "grammar",
            "grammar_not_found",
            "BackLog cannot find its naming rules file.",
            &paths.grammar,
        );
    }

    // Bounded and short: these are liveness probes, not the real per-request
    // timeout used once the pipeline is running, so clamp well below
    // `cfg.sidecar_timeout_secs` regardless of how that's configured.
    let probe_timeout = Duration::from_secs(cfg.sidecar_timeout_secs.clamp(1, 5));

    let sidecar_ok = if sidecar_found {
        let executable = paths.sidecar.clone();
        let models_dir = paths.models_dir.clone();
        match tokio::task::spawn_blocking(move || {
            let sidecar =
                Sidecar::with_timeout(executable, probe_timeout).with_models_dir(models_dir);
            sidecar.ping()
        })
        .await
        {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                push_component_failed(
                    &mut problems,
                    "sidecar",
                    "sidecar_ping_failed",
                    "The part of BackLog that reads your documents did not answer. Restart \
                     BackLog; if it keeps happening, reinstall it.",
                    &error.to_string(),
                );
                false
            }
            Err(error) => {
                push_component_failed(
                    &mut problems,
                    "sidecar",
                    "sidecar_check_failed",
                    "BackLog could not finish testing the part of itself that reads documents.",
                    &error.to_string(),
                );
                false
            }
        }
    } else {
        false
    };

    // Symmetric with the convertd ping above. "The file is on disk" says
    // nothing about whether it runs: a partially quarantined binary, a
    // missing VC++ runtime or a wrong-architecture build all fail here, in
    // two seconds, instead of sixty seconds into the first document's health
    // poll followed by SLM_FAIL on every file thereafter.
    let llama_server_ok = if llama_server_found {
        let executable = paths.llama_server.clone();
        match tokio::task::spawn_blocking(move || probe_llama_server(&executable, probe_timeout))
            .await
        {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                push_component_failed(
                    &mut problems,
                    "llama_server",
                    "llama_server_probe_failed",
                    "The part of BackLog that suggests file names is installed but would not \
                     start. Your antivirus may have blocked it; reinstall BackLog if that does \
                     not explain it.",
                    &error,
                );
                false
            }
            Err(error) => {
                push_component_failed(
                    &mut problems,
                    "llama_server",
                    "llama_server_check_failed",
                    "BackLog could not finish testing the part of itself that suggests names.",
                    &error.to_string(),
                );
                false
            }
        }
    } else {
        false
    };

    // A busy port is the one llama-server failure with no other symptom: the
    // server never becomes healthy, the health poll burns 60 seconds, and
    // every file lands in NeedsReview as SLM_FAIL forever. It is a warning
    // rather than a blocker because the probe is inherently racy and there is
    // no port control in the UI to recover from a false positive.
    if !running {
        for (label, port) in [
            ("", cfg.llama_port),
            (" (backup)", cfg.llama_port.wrapping_add(1)),
        ] {
            if !port_is_free(port) {
                problems.push(RuntimeProblem {
                    field: "llama_port".into(),
                    code: "llama_port_busy".into(),
                    message: format!(
                        "Another program on this computer is already using the network port \
                         BackLog reserves for naming documents{label}. BackLog will keep \
                         working only if that program stops; otherwise ask IT to change \
                         llama_port in backlog.config.json."
                    ),
                    detail: Some(format!("127.0.0.1:{port} is already bound")),
                    severity: ProblemSeverity::Warning,
                    action: None,
                });
            }
        }
    }

    let configured = problems
        .iter()
        .all(|problem| problem.severity != ProblemSeverity::Error)
        && processing.ready
        && outbox_writable
        && quarantine_writable
        && cache_writable
        && sidecar_found
        && sidecar_ok
        && llama_server_found
        && llama_server_ok
        && grammar_found
        && primary_model_found;

    RuntimeStatus {
        configured,
        checked: true,
        running,
        paused: running && paused,
        processing_dir_ready: processing.ready,
        outbox_writable,
        quarantine_writable,
        cache_writable,
        sidecar_found,
        sidecar_ok,
        llama_server_found,
        llama_server_ok,
        grammar_found,
        primary_model_found,
        escalation_model_found,
        offline_runtime: true,
        processing_entry_count: processing.entry_count,
        processing_entry_count_capped: processing.capped,
        processing_sample: processing.sample,
        checked_at: Some(chrono::Utc::now().to_rfc3339()),
        problems,
    }
}

/// The set of config fields `create_missing_dir` is allowed to create, and
/// the only fields a `create_folder` action is ever attached to.
pub const CREATABLE_DIR_FIELDS: &[&str] = &[
    "processing_dir",
    "outbox_dir",
    "quarantine_dir",
    "cache_dir",
];

#[derive(Default)]
struct ProcessingCheck {
    ready: bool,
    entry_count: Option<u64>,
    capped: bool,
    sample: Vec<String>,
}

fn check_processing_dir(problems: &mut Vec<RuntimeProblem>, dir: &Path) -> ProcessingCheck {
    if dir.as_os_str().is_empty() {
        push_error(
            problems,
            "processing_dir",
            "processing_unset",
            "Choose the folder BackLog should watch for new documents.",
        );
        return ProcessingCheck::default();
    }
    if !dir.is_dir() {
        // Outbox, Quarantine and Cache are all `create_dir_all`'d by their
        // writability probe, so an identical class of problem was auto-fixed
        // in three places and reported as an unrecoverable Blocked in the
        // fourth. Offer the same repair here — explicitly, because unlike the
        // other three this folder is shared with OneDrive and a typo should
        // not silently manufacture a second one.
        problems.push(RuntimeProblem {
            field: "processing_dir".into(),
            code: "processing_missing".into(),
            message: format!(
                "The folder BackLog watches for new documents does not exist yet: {}. \
                 Create it, or choose a different folder.",
                dir.display()
            ),
            detail: None,
            severity: ProblemSeverity::Error,
            action: Some(ProblemAction::CreateFolder),
        });
        return ProcessingCheck::default();
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            push_component_failed(
                problems,
                "processing_dir",
                "processing_unreadable",
                "BackLog is not allowed to look inside the folder it watches. Ask IT to give \
                 you read access to it, or choose a folder in your own Documents.",
                &format!("{}: {error}", dir.display()),
            );
            return ProcessingCheck::default();
        }
    };

    let mut count = 0u64;
    let mut sample = Vec::new();
    for entry in entries.flatten() {
        if count >= PROCESSING_SCAN_CAP {
            break;
        }
        count += 1;
        if sample.len() < PROCESSING_SAMPLE_LEN && entry.path().is_file() {
            sample.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    ProcessingCheck {
        ready: true,
        entry_count: Some(count),
        capped: count >= PROCESSING_SCAN_CAP,
        sample,
    }
}

/// A shipped component is "found" only when a real, non-empty file sits where
/// the installer put it.
///
/// Previously this fell back to resolving a bare name against `%PATH%`,
/// mirroring `crate::binary`'s dev-only fallback — so an antivirus engine
/// quarantining `convertd.exe` (routine for PyInstaller-frozen executables)
/// left preflight cheerfully reporting "installed — Ready" while the app was
/// about to hand every document's absolute path to whatever `convertd` a
/// user-writable PATH entry happened to provide. The zero-length test catches
/// the other end of it: `scripts/dev-stubs.sh` stages empty placeholders so
/// the bundle links, and an empty file is not a working sidecar.
fn binary_exists(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// Launch `<exe> --version` and wait a bounded time for it to exit cleanly.
fn probe_llama_server(exe: &Path, timeout: Duration) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let mut command = Command::new(exe);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("{} could not start: {error}", exe.display()))?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{} exited with {status}", exe.display())),
            Ok(None) => {}
            Err(error) => return Err(format!("{} could not be waited on: {error}", exe.display())),
        }
        if std::time::Instant::now() >= deadline {
            // Never leave the probe's own child behind — this is the same
            // orphan class the app exists to avoid.
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{} did not respond within {timeout:?}",
                exe.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Can we bind loopback on this port right now? Purely diagnostic.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
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
            problems.push(RuntimeProblem {
                field: field.into(),
                code: code.into(),
                message: format!(
                    "BackLog cannot write to the {} folder, so it cannot save its results there.",
                    friendly_field(field)
                ),
                detail: Some(message),
                severity: ProblemSeverity::Error,
                action: Some(ProblemAction::CreateFolder),
            });
            false
        }
    }
}

fn friendly_field(field: &str) -> &str {
    match field {
        "processing_dir" => "Processing",
        "outbox_dir" => "Outbox",
        "quarantine_dir" => "Quarantine",
        "cache_dir" => "working",
        other => other,
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
        detail: None,
        severity: ProblemSeverity::Error,
        action: None,
    });
}

/// A component that ships inside the installer is missing: nothing the user
/// configured caused it and nothing they can configure fixes it, so the
/// remedy is always "reinstall" and the path belongs in `detail`.
fn push_missing_component(
    problems: &mut Vec<RuntimeProblem>,
    field: &str,
    code: &str,
    message: &str,
    expected: &Path,
) {
    problems.push(RuntimeProblem {
        field: field.into(),
        code: code.into(),
        message: format!(
            "{message} It is installed with BackLog, so this usually means antivirus removed \
             it — reinstall BackLog, and ask IT to allow it."
        ),
        detail: Some(format!(
            "expected an executable file at {}",
            expected.display()
        )),
        severity: ProblemSeverity::Error,
        action: None,
    });
}

fn push_component_failed(
    problems: &mut Vec<RuntimeProblem>,
    field: &str,
    code: &str,
    message: &str,
    detail: &str,
) {
    problems.push(RuntimeProblem {
        field: field.into(),
        code: code.into(),
        message: message.into(),
        detail: Some(detail.to_string()),
        severity: ProblemSeverity::Error,
        action: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config whose folders all exist and are writable, so a test can knock
    /// out exactly the one thing it is about.
    fn workable_cfg(root: &Path) -> Config {
        for sub in ["proc", "out", "quar", "cache"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Config {
            processing_dir: root.join("proc"),
            outbox_dir: root.join("out"),
            quarantine_dir: root.join("quar"),
            cache_dir: root.join("cache"),
            ..Default::default()
        }
    }

    fn absent_paths(root: &Path) -> RuntimePaths {
        RuntimePaths {
            sidecar: root.join("convertd"),
            llama_server: root.join("llama-server"),
            grammar: root.join("name.gbnf"),
            models_dir: root.join("models"),
            install_dir_error: None,
        }
    }

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

    /// The state every fresh install is in: app installed, models not
    /// downloaded. It used to produce two silent false booleans and no
    /// problem at all.
    #[tokio::test]
    async fn absent_models_produce_a_models_missing_problem_with_a_download_action() {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = workable_cfg(root.path());
        cfg.slm_primary_gguf = root.path().join("models").join("primary.gguf");
        cfg.slm_escalation_gguf = root.path().join("models").join("escalation.gguf");

        let status = run_with(&absent_paths(root.path()), &cfg, false, false).await;

        assert!(!status.configured);
        assert!(!status.primary_model_found && !status.escalation_model_found);
        let problem = status
            .problems
            .iter()
            .find(|p| p.code == "models_missing")
            .expect("a missing model pair must be a named problem, not a silent false boolean");
        assert_eq!(problem.action, Some(ProblemAction::DownloadModels));
        // Both expected locations are in the detail so support can act on it.
        let detail = problem.detail.as_deref().unwrap_or_default();
        assert!(detail.contains("primary.gguf") && detail.contains("escalation.gguf"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_optional_model_is_usable_without_claiming_it_is_installed() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let mut cfg = workable_cfg(root.path());
        cfg.slm_primary_gguf = root.path().join("primary.gguf");
        cfg.slm_escalation_gguf = root.path().join("missing-escalation.gguf");
        std::fs::write(&cfg.slm_primary_gguf, b"model").unwrap();

        let paths = RuntimePaths {
            sidecar: root.path().join("convertd"),
            llama_server: root.path().join("llama-server"),
            grammar: root.path().join("name.gbnf"),
            models_dir: root.path().join("models"),
            install_dir_error: None,
        };
        std::fs::write(
            &paths.sidecar,
            b"#!/bin/sh\nwhile IFS= read -r line; do\n  echo '{\"id\":1,\"ok\":true}'\ndone\n",
        )
        .unwrap();
        std::fs::write(&paths.llama_server, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(&paths.grammar, b"grammar").unwrap();
        std::fs::set_permissions(&paths.sidecar, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&paths.llama_server, std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let status = run_with(&paths, &cfg, false, false).await;

        assert!(status.configured, "primary fallback must remain usable");
        assert!(status.primary_model_found);
        assert!(
            !status.escalation_model_found,
            "the optional 1.7B row must remain honest"
        );
        let fallback = status
            .problems
            .iter()
            .find(|problem| problem.code == "escalation_model_missing_using_primary")
            .expect("degraded readiness must explain the safe primary fallback");
        assert_eq!(fallback.severity, ProblemSeverity::Warning);
        assert!(fallback
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("primary model will safely handle escalation attempts"));
    }

    #[test]
    fn summary_names_the_failed_checks_when_no_problem_carries_a_message() {
        let mut status = RuntimeStatus::unchecked(false, false);
        status.problems.clear();
        status.checked = true;
        status.processing_dir_ready = true;
        status.outbox_writable = true;
        status.quarantine_writable = true;
        status.cache_writable = true;
        status.sidecar_found = true;
        status.sidecar_ok = true;
        status.llama_server_found = true;
        status.llama_server_ok = true;
        status.grammar_found = true;

        let summary = status.summary();
        assert!(
            summary.contains("everyday model file") && summary.contains("backup model file"),
            "summary must name the failing checks, got: {summary}"
        );
        assert!(!summary.contains("runtime preflight did not pass"));
    }

    #[tokio::test]
    async fn a_missing_processing_folder_is_offered_as_creatable() {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = workable_cfg(root.path());
        cfg.processing_dir = root.path().join("never-made");

        let status = run_with(&absent_paths(root.path()), &cfg, false, false).await;

        assert!(!status.processing_dir_ready);
        let problem = status
            .problems
            .iter()
            .find(|p| p.code == "processing_missing")
            .expect("a missing Processing folder must be repairable, like the other three");
        assert_eq!(problem.action, Some(ProblemAction::CreateFolder));
        assert!(CREATABLE_DIR_FIELDS.contains(&problem.field.as_str()));
    }

    #[tokio::test]
    async fn processing_folder_contents_are_reported_so_the_user_can_confirm_the_choice() {
        let root = tempfile::tempdir().unwrap();
        let cfg = workable_cfg(root.path());
        std::fs::write(cfg.processing_dir.join("offer letter.pdf"), b"x").unwrap();
        std::fs::write(cfg.processing_dir.join("invoice.docx"), b"x").unwrap();

        let status = run_with(&absent_paths(root.path()), &cfg, false, false).await;

        assert_eq!(status.processing_entry_count, Some(2));
        assert!(!status.processing_entry_count_capped);
        assert_eq!(status.processing_sample.len(), 2);
    }

    /// `scripts/dev-stubs.sh` stages zero-byte placeholders so the bundle
    /// links; preflight must not certify one as an installed sidecar, and it
    /// must never accept a bare name resolved off %PATH%.
    #[test]
    fn binary_exists_rejects_zero_length_stubs_and_bare_names() {
        let root = tempfile::tempdir().unwrap();
        let stub = root.path().join("convertd");
        std::fs::write(&stub, b"").unwrap();
        assert!(!binary_exists(&stub));

        std::fs::write(&stub, b"#!/bin/sh\n").unwrap();
        assert!(binary_exists(&stub));

        assert!(!binary_exists(Path::new("convertd")));
        assert!(!binary_exists(Path::new("sh")));
    }

    #[test]
    fn llama_server_probe_reports_a_binary_that_cannot_start() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("llama-server");
        let error = probe_llama_server(&missing, Duration::from_secs(1)).unwrap_err();
        assert!(error.contains("could not start"), "got: {error}");
    }

    #[cfg(unix)]
    #[test]
    fn llama_server_probe_kills_a_binary_that_never_exits() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let hung = root.path().join("llama-server");
        std::fs::write(&hung, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&hung, std::fs::Permissions::from_mode(0o755)).unwrap();

        let error = probe_llama_server(&hung, Duration::from_millis(200)).unwrap_err();
        assert!(error.contains("did not respond"), "got: {error}");
    }

    #[cfg(unix)]
    #[test]
    fn llama_server_probe_accepts_a_binary_that_answers_version() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let ok = root.path().join("llama-server");
        std::fs::write(&ok, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();
        probe_llama_server(&ok, Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn a_bound_port_is_reported_as_busy() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free(port));
        drop(listener);
        assert!(port_is_free(port));
    }

    #[test]
    fn join_and_reads_as_a_sentence() {
        assert_eq!(join_and(&["a"]), "a");
        assert_eq!(join_and(&["a", "b"]), "a and b");
        assert_eq!(join_and(&["a", "b", "c"]), "a, b and c");
    }
}
