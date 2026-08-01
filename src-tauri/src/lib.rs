mod config;
mod dbkey;
mod filter;
mod identity;
mod ledger;
mod logging;
mod manifest;
mod model_download;
mod pipeline;
mod preflight;
mod routing;
mod sidecar;
mod slm;
mod watcher;

// The deterministic trust core lives in its own dependency-light crate so it
// tests without any Tauri build; re-export it under the crate paths the rest
// of the code already uses (crate::checker, crate::harvest).
pub use backlog_core::{checker, harvest};

use config::Config;
use ledger::Ledger;
use pipeline::Pipeline;
use preflight::RuntimeStatus;
use sidecar::Sidecar;
use slm::SlmLane;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, RunEvent, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

struct AppState {
    cfg_path: PathBuf,
    cfg: Mutex<Config>,
    /// Where the cache lives when the operator has not (and cannot) set it:
    /// there is no cache control anywhere in the UI, so an empty or unusable
    /// value self-heals to this instead of becoming an unfixable Blocked row.
    default_cache_dir: PathBuf,
    log_path: PathBuf,
    ledger: Arc<Ledger>,
    pipeline: Mutex<Option<Arc<Pipeline>>>,
    last_preflight: Mutex<Option<RuntimeStatus>>,
}

/// Something that went wrong before the window existed. A windowed Windows
/// build that aborts in `setup` shows the user nothing at all — the app simply
/// does not appear when double-clicked — and the dialog plugin dispatches
/// through `run_on_main_thread`, which does nothing until the event loop is
/// running. So failures are recorded here and surfaced from `RunEvent::Ready`.
struct StartupNotice {
    fatal: bool,
    message: String,
}

/// Current pipeline running/paused state, read fresh from `AppState` on every
/// call so cached preflight results can be overlaid with it without going
/// stale between an explicit `run_preflight` and a later `set_paused`.
fn runtime_flags(state: &AppState) -> (bool, bool) {
    let pipeline = state.pipeline.lock().unwrap();
    match pipeline.as_ref() {
        Some(pipeline) => (
            true,
            pipeline.paused.load(std::sync::atomic::Ordering::Relaxed),
        ),
        None => (false, false),
    }
}

pub(crate) fn resource(app: &tauri::AppHandle, rel: &str) -> PathBuf {
    app.path()
        .resolve(
            format!("resources/{rel}"),
            tauri::path::BaseDirectory::Resource,
        )
        .unwrap_or_else(|_| PathBuf::from(format!("resources/{rel}")))
}

fn reveal_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

pub(crate) fn binary(app: &tauri::AppHandle, name: &str) -> Result<PathBuf, String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    // llama-server still ships through `externalBin`: Tauri copies its single
    // exe next to the main executable, so the generic exe_dir candidates in
    // `resolve_binary` already find it and `resource(app, name)` is a
    // harmless last resort. convertd cannot travel that way any more --
    // PyInstaller's `--onedir` output is a directory tree (`convertd.exe`
    // plus a `_internal/` folder of DLLs and data files PyInstaller loads at
    // startup), and `externalBin` only ever stages a single file. It ships
    // through `bundle.resources` instead (tauri.conf.json's
    // `"binaries/convertd/": "convertd/"`), which lands one path segment
    // deeper than an externalBin exe: on Windows `resource_dir()` *is*
    // `exe_dir` (the directory containing the main executable — see Tauri's
    // `PathResolver::resource_dir` docs), so the installed layout is
    // `<exe_dir>/convertd/convertd.exe`, not `<exe_dir>/convertd.exe`.
    // `cargo run` never runs the resource-copy step that produces that
    // layout, so the dev fallback instead points straight at the onedir tree
    // scripts/build-sidecar.ps1 stages under src-tauri/binaries/convertd/.
    let resource_candidate = if name == "convertd" {
        let convertd_exe = if cfg!(windows) {
            "convertd.exe"
        } else {
            "convertd"
        };
        if cfg!(debug_assertions) {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("binaries")
                .join("convertd")
                .join(convertd_exe)
        } else {
            match exe_dir.as_deref() {
                Some(dir) => dir.join("convertd").join(convertd_exe),
                None => Path::new("convertd").join(convertd_exe),
            }
        }
    } else {
        resource(app, name).with_file_name(name)
    };
    // convertd is resolved against that path and nothing else. `resolve_binary`
    // tries `<exe_dir>/<name>.exe` *before* the resource candidate, which is
    // right for an externalBin sidecar and actively harmful here: upgrading
    // over a `--onefile` install leaves a stale 248 MB `convertd.exe` sitting
    // beside the app, and that stale copy would win. The app would keep
    // working, so nothing would look broken — it would just go on unpacking a
    // quarter of a gigabyte on every launch, which is the entire failure this
    // layout exists to remove. A silent revert is worse than a loud miss.
    if name == "convertd" {
        if resource_candidate.is_file() {
            return Ok(resource_candidate);
        }
        if cfg!(debug_assertions) {
            return Ok(PathBuf::from(name));
        }
        // Name the file the installer should have written, so the failure
        // points somewhere the user can actually look.
        return Ok(resource_candidate);
    }
    // `cargo tauri dev` runs the binary out of target/debug, where the
    // sidecars are not staged; a shipped build must never take that path.
    resolve_binary(
        exe_dir.as_deref(),
        &resource_candidate,
        name,
        cfg!(debug_assertions),
    )
}

/// Locate a bundled sidecar executable.
///
/// `allow_path_fallback` is a developer convenience and nothing else. In a
/// release build the last resort is the path the installer wrote, so a spawn
/// failure names a file the user can look for; returning a bare `"convertd"`
/// would instead ask the OS to resolve it against `%PATH%`, and every
/// user-writable PATH entry on a standard Windows profile then becomes a way
/// to receive the absolute path of every document this app touches and hand
/// back the text the pipeline trusts. Antivirus quarantining a
/// PyInstaller-frozen executable is routine, so that is not a hypothetical
/// trigger.
///
/// `exe_dir` is an `Option` because that is what makes the invariant real: a
/// failed `current_exe()` used to collapse to `PathBuf::new()`, and
/// `"".join("convertd.exe")` is the bare relative name this function exists to
/// never return. There is no safe path to construct in that case, so it is an
/// error the caller reports rather than a value it spawns.
fn resolve_binary(
    exe_dir: Option<&Path>,
    resource_candidate: &Path,
    name: &str,
    allow_path_fallback: bool,
) -> Result<PathBuf, String> {
    // Tauri externalBin sidecars sit next to the app binary with a target
    // triple suffix in dev; resolve both layouts.
    let windows_name = format!("{name}.exe");
    let mut candidates = Vec::new();
    if let Some(dir) = exe_dir {
        candidates.push(dir.join(name));
        candidates.push(dir.join(&windows_name));
    }
    candidates.push(resource_candidate.to_path_buf());
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    if allow_path_fallback {
        return Ok(PathBuf::from(name));
    }
    let Some(dir) = exe_dir else {
        return Err(format!(
            "BackLog could not work out where it is installed, so it cannot find the part of \
             itself called {name}. Reinstalling BackLog fixes this."
        ));
    };
    Ok(if cfg!(windows) {
        dir.join(&windows_name)
    } else {
        dir.join(name)
    })
}

/// The ledger key is a content hash, optionally with a duplicate suffix.
/// Anything that isn't hex/'-' is rejected so a crafted value can't traverse
/// out of the cache dir through a `{sha256}.md` join, or address a row it was
/// never handed.
fn is_ledger_key(id: &str) -> bool {
    !id.is_empty() && id.len() <= 90 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

#[tauri::command]
fn get_config(state: tauri::State<AppState>) -> Config {
    state.cfg.lock().unwrap().clone()
}

/// Windows accepts both separators and normally resolves paths
/// case-insensitively. Compare that identity even in platform-independent
/// tests so a config copied from another machine is covered.
#[cfg(any(windows, test))]
fn windows_paths_equivalent(left: &Path, right: &Path) -> bool {
    fn key(path: &Path) -> String {
        let mut key = String::with_capacity(path.as_os_str().len());
        for ch in path.to_string_lossy().chars() {
            if ch == '\\' || ch == '/' {
                key.push('\\');
            } else {
                for folded in ch.to_lowercase() {
                    key.push(folded);
                }
            }
        }
        while key.len() > 3 && key.ends_with('\\') {
            key.pop();
        }
        key
    }
    key(left) == key(right)
}

/// Model destinations collide when they are spelled identically, resolve to
/// the same existing target, or are Windows-equivalent spellings.
fn model_paths_collide(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    if let (Ok(left), Ok(right)) = (left.canonicalize(), right.canonicalize()) {
        #[cfg(windows)]
        if windows_paths_equivalent(&left, &right) {
            return true;
        }
        #[cfg(not(windows))]
        if left == right {
            return true;
        }
    }
    #[cfg(windows)]
    return windows_paths_equivalent(left, right);
    #[cfg(not(windows))]
    false
}

/// Repair the v0.4.4 state where normalization persisted the installed
/// primary path into both model fields. The desired optional-model path is
/// always the canonical 1.7B destination, never the runtime fallback.
fn migrate_colliding_model_paths(cfg: &mut Config, models_dir: &Path) -> Result<bool, String> {
    if !model_paths_collide(&cfg.slm_primary_gguf, &cfg.slm_escalation_gguf) {
        return Ok(false);
    }
    let canonical_escalation = models_dir.join(model_download::ESCALATION_GGUF_NAME);
    if model_paths_collide(&cfg.slm_primary_gguf, &canonical_escalation) {
        return Err(
            "The everyday and optional model paths must be different. Choose the 0.6B file for \
             the everyday model; BackLog reserves its canonical 1.7B path for the optional model."
                .into(),
        );
    }
    cfg.slm_escalation_gguf = canonical_escalation;
    Ok(true)
}

/// Resolve installed-app model paths and durably repair the v0.4.4 collision.
///
/// The migration is persisted at this seam rather than waiting for the rest of
/// startup. A later failure opening the ledger must not make the next launch
/// rediscover the same broken configuration.
fn repair_and_persist_startup_model_paths(
    cfg_path: &Path,
    cfg: &mut Config,
    models_dir: &Path,
) -> Result<(), String> {
    cfg.slm_primary_gguf = model_download::resolve_configured_model_path(
        models_dir,
        &cfg.slm_primary_gguf,
        model_download::PRIMARY_GGUF_NAME,
    );
    cfg.slm_escalation_gguf = model_download::resolve_configured_model_path(
        models_dir,
        &cfg.slm_escalation_gguf,
        model_download::ESCALATION_GGUF_NAME,
    );
    if migrate_colliding_model_paths(cfg, models_dir)? {
        cfg.save(cfg_path).map_err(|_| {
            "BackLog could not save its repaired model settings. Check that its app-data folder \
             is writable, then start BackLog again."
                .to_string()
        })?;
    }
    Ok(())
}

/// Normalize, validate, and persist an incoming configuration.
///
/// Split out of the command so the policy — not the IPC plumbing — is what
/// the tests exercise. Validation happens *here* rather than only at preflight
/// so a nested Outbox-inside-Processing is refused at the point of the
/// mistake, while the user is still looking at the folder they just picked.
fn apply_config(
    cfg_path: &Path,
    default_cache_dir: &Path,
    mut cfg: Config,
) -> Result<Config, String> {
    cfg.normalize();
    cfg.clamp_resources_to_machine();
    if cfg.cache_dir.as_os_str().is_empty() {
        cfg.cache_dir = default_cache_dir.to_path_buf();
    }
    let models_dir = default_cache_dir
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join("models");
    migrate_colliding_model_paths(&mut cfg, &models_dir)?;
    cfg.validate()?;
    cfg.save(cfg_path).map_err(|e| e.to_string())?;
    Ok(cfg)
}

/// Tell the log scrubber which folders on this machine are the sensitive ones.
///
/// The Processing root and the names under it are the whole point: they are
/// HR-shaped ("2024 Terminations"), and the persistent log file is plaintext
/// on a product that SQLCipher-encrypts its ledger for exactly that reason.
fn register_sensitive_paths(cfg: &Config) {
    logging::add_sensitive_roots([
        cfg.processing_dir.clone(),
        cfg.outbox_dir.clone(),
        cfg.quarantine_dir.clone(),
        cfg.cache_dir.clone(),
    ]);
}

#[tauri::command]
fn set_config(state: tauri::State<AppState>, cfg: Config) -> Result<(), String> {
    let saved = apply_config(&state.cfg_path, &state.default_cache_dir, cfg)?;
    register_sensitive_paths(&saved);
    *state.cfg.lock().unwrap() = saved;
    // Settings changed underneath it; the last preflight result no longer
    // describes the current configuration, so drop it back to fail-closed
    // "unchecked" rather than let a stale pass linger in the UI.
    *state.last_preflight.lock().unwrap() = None;
    Ok(())
}

/// The config as preflight and the pipeline should see it, with the
/// app-managed cache repaired if it has become unusable. Persisted, so the
/// repair survives a restart.
fn healed_cfg(state: &AppState) -> Config {
    let mut cfg = state.cfg.lock().unwrap().clone();
    let cache_usable =
        !cfg.cache_dir.as_os_str().is_empty() && std::fs::create_dir_all(&cfg.cache_dir).is_ok();
    if !cache_usable && cfg.cache_dir != state.default_cache_dir {
        log::warn!("cache folder unusable; falling back to the app-managed cache");
        cfg.cache_dir = state.default_cache_dir.clone();
        let _ = std::fs::create_dir_all(&cfg.cache_dir);
        let _ = cfg.save(&state.cfg_path);
        register_sensitive_paths(&cfg);
        *state.cfg.lock().unwrap() = cfg.clone();
    }
    cfg
}

#[tauri::command]
fn get_runtime_status(state: tauri::State<AppState>) -> RuntimeStatus {
    let (running, paused) = runtime_flags(&state);
    state
        .last_preflight
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| RuntimeStatus::unchecked(running, paused))
        .with_runtime(running, paused)
}

#[tauri::command]
async fn run_preflight(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<RuntimeStatus, String> {
    let cfg = healed_cfg(&state);
    let (running, paused) = runtime_flags(&state);
    let status = preflight::run(&app, &cfg, running, paused).await;
    *state.last_preflight.lock().unwrap() = Some(status.clone());
    Ok(status)
}

/// Create one of the four configured folders on the user's behalf. Only the
/// four config fields are addressable — the path itself never comes from the
/// webview.
#[tauri::command]
fn create_missing_dir(state: tauri::State<AppState>, field: String) -> Result<(), String> {
    create_missing_dir_inner(&state, &field)
}

fn create_missing_dir_inner(state: &AppState, field: &str) -> Result<(), String> {
    if !preflight::CREATABLE_DIR_FIELDS.contains(&field) {
        return Err(format!("{field} is not a folder BackLog can create."));
    }
    let cfg = state.cfg.lock().unwrap().clone();
    let dir = match field {
        "processing_dir" => cfg.processing_dir,
        "outbox_dir" => cfg.outbox_dir,
        "quarantine_dir" => cfg.quarantine_dir,
        _ => cfg.cache_dir,
    };
    if dir.as_os_str().is_empty() {
        return Err("Choose a folder first.".into());
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    *state.last_preflight.lock().unwrap() = None;
    Ok(())
}

/// Parse an optional state filter coming from the webview. An unknown value is
/// an error rather than a silent "no filter", so a typo in the UI cannot show
/// the operator the whole backfill while claiming it is filtered.
fn parse_state_filter(raw: Option<&str>) -> Result<Option<ledger::JobState>, String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(name) => ledger::JobState::parse(name)
            .map(Some)
            .ok_or_else(|| format!("'{name}' is not a job state.")),
    }
}

/// Paged, filtered listing. A multi-thousand-file backfill cannot be reviewed
/// through an unbounded "most recent 500".
#[tauri::command]
fn list_jobs(
    state: tauri::State<AppState>,
    query: Option<String>,
    job_state: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Vec<ledger::Job>, String> {
    let filter = parse_state_filter(job_state.as_deref())?;
    state
        .ledger
        .search_jobs(
            query.as_deref(),
            filter,
            limit.unwrap_or(500).min(2000),
            offset.unwrap_or(0),
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn count_jobs(
    state: tauri::State<AppState>,
    query: Option<String>,
    job_state: Option<String>,
) -> Result<i64, String> {
    let filter = parse_state_filter(job_state.as_deref())?;
    state
        .ledger
        .count_jobs(query.as_deref(), filter)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_flagged(
    state: tauri::State<AppState>,
    limit: Option<usize>,
    offset: Option<usize>,
    reason: Option<String>,
    oldest_first: Option<bool>,
) -> Result<Vec<ledger::Job>, String> {
    state
        .ledger
        .list_flagged_paged(
            reason.as_deref(),
            oldest_first.unwrap_or(false),
            limit.unwrap_or(500).min(2000),
            offset.unwrap_or(0),
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn count_flagged(state: tauri::State<AppState>, reason: Option<String>) -> Result<i64, String> {
    state
        .ledger
        .count_flagged(reason.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_flagged_job(
    state: tauri::State<AppState>,
    sha256: String,
) -> Result<Option<ledger::Job>, String> {
    if !is_ledger_key(&sha256) {
        return Err("invalid id".into());
    }
    state
        .ledger
        .get(&sha256)
        .map(|job| job.filter(|job| job.state == ledger::JobState::Flagged))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_flag_reasons(state: tauri::State<AppState>) -> Result<Vec<String>, String> {
    state.ledger.flagged_reasons().map_err(|e| e.to_string())
}

#[tauri::command]
fn get_stats(state: tauri::State<AppState>) -> Result<serde_json::Value, String> {
    let mut stats = state.ledger.stats().map_err(|e| e.to_string())?;
    // Files finished in the last hour: the only honest basis for "will this
    // backfill be done today?", and it is already in the ledger.
    if let Some(map) = stats.as_object_mut() {
        let per_hour = state.ledger.throughput(60).unwrap_or(0);
        map.insert("per_hour".into(), serde_json::json!(per_hour));
    }
    Ok(stats)
}

/// The forensic trail for one job — per-attempt OCR confidence, checker
/// rejection codes, span-mismatch re-prompts. It is written at a dozen sites
/// and, until now, read at none.
#[tauri::command]
fn get_events(
    state: tauri::State<AppState>,
    sha256: String,
    limit: Option<usize>,
) -> Result<Vec<ledger::Event>, String> {
    get_events_inner(&state, &sha256, limit)
}

/// One job's events, newest first. The limit is clamped because it arrives
/// from the webview and the events table is the largest thing in the ledger.
fn get_events_inner(
    state: &AppState,
    sha256: &str,
    limit: Option<usize>,
) -> Result<Vec<ledger::Event>, String> {
    if !is_ledger_key(sha256) {
        return Err("invalid id".into());
    }
    state
        .ledger
        .events_for(sha256, limit.unwrap_or(100).min(MAX_EVENTS_PER_CALL))
        .map_err(|e| e.to_string())
}

const MAX_EVENTS_PER_CALL: usize = 500;

/// The manifest that tells Flow 2 a human looked at this file and decided it
/// is not worth filing. `ReviewState = Dismissed` already exists downstream;
/// the app simply had no way to produce it, so junk in the review queue had no
/// terminal state and Needs Review only ever grew.
fn dismissed_manifest(job: &ledger::Job, note: &str) -> manifest::Manifest {
    let relpath = job
        .original_relpath
        .clone()
        .unwrap_or_else(|| job.original_name.clone());
    let reason = match note.trim() {
        "" => "DISMISSED:no reason given".to_string(),
        note => format!("DISMISSED:{note}"),
    };
    manifest::Manifest {
        schema: manifest::MANIFEST_SCHEMA_VERSION,
        manifest_id: identity::instance_id(&job.sha256, &identity::normalize_relpath(&relpath)),
        sha256: job.sha256.clone(),
        status: "dismissed".into(),
        original_name: job.original_name.clone(),
        original_relpath: relpath,
        new_filename: None,
        description: None,
        date: None,
        date_source: None,
        doc_type: job.doc_type.clone(),
        language: job.language.clone(),
        duplicate_of: None,
        soft_flags: vec![],
        flag_reason: Some(reason),
        // Reproducibility is per job, and the job already recorded what named
        // it; a dismissal must not silently claim a different provenance.
        model_versions: job
            .model_versions
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_else(|| serde_json::json!({})),
        processed_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// Retire a flagged file the operator has judged to be junk: index it as
/// dismissed downstream, drop its cached document text, and leave the original
/// in quarantine so the decision is reversible by hand.
///
/// The manifest is written first and the ledger only moves once it lands — a
/// dismissal the operator can see but Flow 2 never hears about is exactly the
/// divergence this ordering exists to prevent.
#[tauri::command]
fn dismiss(state: tauri::State<AppState>, sha256: String, note: String) -> Result<(), String> {
    dismiss_inner(&state, &sha256, &note)
}

fn dismiss_inner(state: &AppState, sha256: &str, note: &str) -> Result<(), String> {
    if !is_ledger_key(sha256) {
        return Err("invalid id".into());
    }
    let job = state
        .ledger
        .get(sha256)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "BackLog has no record of that file.".to_string())?;
    // Dismissal is a decision about something sitting in the review queue.
    // The ledger's transition table lets any non-terminal state reach
    // Dismissed (a losing worker must always be able to retire a row), so
    // without this gate a stale UI row or a double-click could write a
    // dismissed manifest for a file a worker is actively converting — and
    // then delete the cached document text out from under it.
    if job.state != ledger::JobState::Flagged {
        return Err(
            "Only files waiting in Needs Review can be dismissed. This one has already moved on; \
             refresh the list to see where it went."
                .into(),
        );
    }
    let cfg = state.cfg.lock().unwrap().clone();

    manifest::write_manifest(&cfg.manifests_dir(), &dismissed_manifest(&job, note))
        .map_err(|e| format!("Could not record the dismissal for SharePoint: {e}"))?;

    if !state
        .ledger
        .set_state(sha256, ledger::JobState::Dismissed)
        .map_err(|e| e.to_string())?
    {
        return Err("That file has already moved on; refresh and try again.".into());
    }
    let _ = state
        .ledger
        .log_event(sha256, "dismiss", "dismissed by operator");
    // The document text goes even when retain_cache is set: a dismissal is a
    // decision not to keep this document, not a corpus contribution.
    let _ = std::fs::remove_file(cfg.cache_dir.join(format!("{sha256}.md")));
    log::info!("job dismissed by operator");
    Ok(())
}

/// Put a file back at the head of the ladder after the operator has fixed
/// whatever broke it (installed a codec, unlocked a PDF, freed disk).
#[tauri::command]
fn reprocess(state: tauri::State<AppState>, sha256: String) -> Result<(), String> {
    reprocess_inner(&state, &sha256)
}

fn reprocess_inner(state: &AppState, sha256: &str) -> Result<(), String> {
    if !is_ledger_key(sha256) {
        return Err("invalid id".into());
    }
    let job = state
        .ledger
        .get(sha256)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "BackLog has no record of that file.".to_string())?;
    if job.state != ledger::JobState::Flagged {
        return Err("Only files in Needs Review can be sent through another try.".into());
    }
    let cfg = state.cfg.lock().unwrap().clone();

    // Restore to the *relative* location it came from, so its identity (and
    // therefore its manifest id) is the one the flagged manifest already used.
    let relpath = job
        .original_relpath
        .clone()
        .unwrap_or_else(|| job.original_name.clone());
    let destination = cfg.processing_dir.join(&relpath);
    let quarantined = job.quarantine_path.clone().map(PathBuf::from);

    // Move the file back before touching the ledger: if this fails nothing has
    // changed, and if the ledger reset later fails the watcher simply
    // re-observes a still-flagged job at its original path, which is a no-op.
    if let Some(source) = quarantined.filter(|p| p.exists()) {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if std::fs::rename(&source, &destination).is_err() {
            // Cross-volume rename fails; never lose the file.
            std::fs::copy(&source, &destination)
                .map_err(|e| format!("Could not move the file back for another try: {e}"))?;
            let _ = std::fs::remove_file(&source);
        }
    } else if !destination.exists() {
        return Err("The original file is no longer in the quarantine folder.".into());
    }

    if !state
        .ledger
        .reset_for_reprocess(sha256)
        .map_err(|e| e.to_string())?
    {
        return Err("That review item has already changed; refresh and try again.".into());
    }
    let _ = state
        .ledger
        .log_event(sha256, "reprocess", "retry requested by operator");

    // Running: enqueue now. Stopped: the watcher's startup sweep picks it up
    // the next time Start is pressed, which is what the file being back in
    // Processing already means.
    let pipeline = state.pipeline.lock().unwrap().clone();
    if let Some(pipeline) = pipeline {
        tauri::async_runtime::spawn(pipeline.process_file(destination));
    }
    log::info!("job re-queued by operator");
    Ok(())
}

#[tauri::command]
fn get_evidence(state: tauri::State<AppState>, sha256: String) -> Result<String, String> {
    if !is_ledger_key(&sha256) {
        return Err("invalid id".into());
    }
    let cfg = state.cfg.lock().unwrap().clone();
    let p = cfg.cache_dir.join(format!("{sha256}.md"));
    std::fs::read_to_string(p).map_err(|e| e.to_string())
}

/// Show a quarantined original in the OS file manager, selected.
///
/// The path is looked up in the ledger and re-derived from the configured
/// quarantine folder; a path from the webview is never accepted, and the
/// result is checked to still be inside quarantine before it is handed to the
/// shell. Spawning `explorer.exe` directly is the same mechanism the sidecars
/// already use, so this needs no plugin and no new capability grant.
#[tauri::command]
fn reveal_quarantined(state: tauri::State<AppState>, sha256: String) -> Result<(), String> {
    reveal_in_file_manager(&quarantined_path(&state, &sha256)?)
}

fn quarantined_path(state: &AppState, sha256: &str) -> Result<PathBuf, String> {
    if !is_ledger_key(sha256) {
        return Err("invalid id".into());
    }
    let job = state
        .ledger
        .get(sha256)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "BackLog has no record of that file.".to_string())?;
    let cfg = state.cfg.lock().unwrap().clone();
    // `flag()` records where it actually put the file — two flagged scan.pdf
    // files land under different quarantine names on purpose, so the leaf name
    // must never be used to reconstruct this.
    let target = PathBuf::from(job.quarantine_path.ok_or_else(|| {
        "BackLog did not quarantine that file, so there is nothing to show.".to_string()
    })?);
    // The path comes from the ledger, never from the webview, and is still
    // checked to be inside quarantine before it reaches a shell command.
    if !target.starts_with(&cfg.quarantine_dir) || !target.exists() {
        return Err("That file is no longer in the quarantine folder.".into());
    }
    Ok(target)
}

#[tauri::command]
fn open_logs_folder(state: tauri::State<AppState>) -> Result<(), String> {
    reveal_in_file_manager(&state.log_path)
}

fn reveal_in_file_manager(target: &Path) -> Result<(), String> {
    let mut command = if cfg!(windows) {
        let mut c = std::process::Command::new("explorer.exe");
        c.arg(format!("/select,{}", target.display()));
        c
    } else {
        // Development convenience only; the shipped bundle is Windows-only.
        let mut c = std::process::Command::new("xdg-open");
        c.arg(target.parent().unwrap_or(target));
        c
    };
    command
        .spawn()
        .map(|_| ())
        // explorer.exe returns a non-zero exit code even on success, so the
        // spawn is the only thing worth checking.
        .map_err(|e| format!("Could not open the folder: {e}"))
}

/// Everything a support conversation needs, in one copyable payload.
#[derive(serde::Serialize)]
struct Diagnostics {
    app_version: String,
    platform: String,
    log_path: String,
    runtime: RuntimeStatus,
    /// Folder paths reduced to a drive letter plus depth — this payload is
    /// meant to be pasted into an email, and the folder names on this
    /// appliance are themselves HR-shaped.
    config: serde_json::Value,
    sidecar_versions: serde_json::Value,
    job_counts: serde_json::Value,
    log_tail: Vec<String>,
}

#[tauri::command]
async fn get_diagnostics(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Diagnostics, String> {
    let cfg = state.cfg.lock().unwrap().clone();
    let runtime = get_runtime_status(state.clone());
    let job_counts = state
        .ledger
        .stats()
        .unwrap_or_else(|_| serde_json::json!({}));
    // Scrubbed by `tail` itself: this is the field the user is invited to
    // paste into an email, and the file it comes from is written by a pipeline
    // whose error text quotes document paths and model proposals.
    let log_tail = logging::tail(&state.log_path, 200);
    // Same reasoning one level up — on Windows this path contains the account
    // name. "Open logs folder" is how anyone actually gets to the file.
    let log_path = logging::redact_path(&state.log_path);

    // Ask the sidecar who it is, bounded, off the async runtime — the same
    // probe preflight uses. A missing or broken sidecar just yields nulls.
    let models_dir = model_download::resolve_models_dir(&app);
    let sidecar_versions = match binary(&app, "convertd") {
        Ok(sidecar_exe) => tokio::task::spawn_blocking(move || {
            Sidecar::with_timeout(sidecar_exe, Duration::from_secs(5))
                .with_models_dir(models_dir)
                .versions()
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
        })
        .await
        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
        Err(error) => serde_json::json!({ "error": error }),
    };

    Ok(Diagnostics {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        log_path,
        runtime,
        config: redacted_config(&cfg),
        sidecar_versions,
        job_counts,
        log_tail,
    })
}

fn redacted_config(cfg: &Config) -> serde_json::Value {
    serde_json::json!({
        "processing_dir": logging::redact_path(&cfg.processing_dir),
        "outbox_dir": logging::redact_path(&cfg.outbox_dir),
        "quarantine_dir": logging::redact_path(&cfg.quarantine_dir),
        "cache_dir": logging::redact_path(&cfg.cache_dir),
        "primary_model": cfg.slm_primary_gguf.file_name().map(|n| n.to_string_lossy().into_owned()),
        "escalation_model": cfg.slm_escalation_gguf.file_name().map(|n| n.to_string_lossy().into_owned()),
        "llama_port": cfg.llama_port,
        "slm_parallel": cfg.slm_parallel,
        "convert_workers": cfg.convert_workers,
        "sidecar_timeout_secs": cfg.sidecar_timeout_secs,
        "manifest_emit_per_min": cfg.manifest_emit_per_min,
        "max_stage_attempts": cfg.max_stage_attempts,
        "per_file_wall_clock_secs": cfg.per_file_wall_clock_secs,
        "retain_cache": cfg.retain_cache,
        "cache_ttl_days": cfg.cache_ttl_days,
        "ettin_enabled": !cfg.ettin_model_dir.is_empty(),
    })
}

#[tauri::command]
async fn resubmit(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    sha256: String,
    date: String,
    subject: String,
    description: String,
) -> Result<(), String> {
    let running = state.pipeline.lock().unwrap().clone();
    let pipeline = match running {
        Some(pipeline) => pipeline,
        None => review_only_pipeline(&app, &state).await?,
    };
    pipeline
        .resubmit(&sha256, date, subject, description)
        .await
        .map_err(friendly_resubmit_error)
}

/// Translate the one failure a reviewer can actually hit into something a
/// non-technical user can act on.
///
/// Manifest v3 refuses an `ok` manifest with empty `model_versions`, and
/// `model_versions` is empty exactly when convertd could not be asked — the
/// antivirus-quarantine case. Without this the Approve button answers with the
/// raw string "ok manifest has no model provenance (model_versions is empty)".
fn friendly_resubmit_error(error: anyhow::Error) -> String {
    let text = error.to_string();
    if text.contains("model_versions") {
        return "BackLog could not record which model version named this file, because the part \
                of it that reads documents is not answering. Press Check in Settings, then try \
                approving again."
            .into();
    }
    text
}

/// A throwaway `Pipeline` over the same config and ledger, for review actions
/// taken before (or after) the pipeline is started.
///
/// Approving a correction is a ledger + checker + manifest operation: it never
/// touches convertd or the SLM. Requiring Start first made the natural
/// first-launch sequence — open the app, see the red Needs Review badge, go
/// fix them — fail with the raw string "pipeline not started", *after* the
/// user had read the document and typed three fields. Reusing `Pipeline`
/// rather than reimplementing resubmit keeps the review path bit-identical to
/// the live one. Nothing here starts a model server; `Pipeline::new` probes
/// convertd once for `model_versions` and falls back to `{}` when it cannot,
/// which is why this is built off the async runtime.
///
/// That probe is clamped to the same short bound preflight uses rather than
/// `sidecar_timeout_secs` (45s by default): on the machine where this matters —
/// convertd quarantined or hung — every single approval would otherwise block
/// for three quarters of a minute before failing, and a reviewer working a
/// several-hundred-item queue pays that per click.
async fn review_only_pipeline(
    app: &tauri::AppHandle,
    state: &AppState,
) -> Result<Arc<Pipeline>, String> {
    let cfg = state.cfg.lock().unwrap().clone();
    let ledger = state.ledger.clone();
    let models_dir = model_download::resolve_models_dir(app);
    let grammar = std::fs::read_to_string(resource(app, "name.gbnf")).unwrap_or_default();
    let sidecar_exe = binary(app, "convertd")?;
    let llama_exe = binary(app, "llama-server")?;
    let app = app.clone();
    tokio::task::spawn_blocking(move || {
        let sidecar = Arc::new(
            Sidecar::with_timeout(sidecar_exe, review_probe_timeout(&cfg))
                .with_models_dir(models_dir),
        );
        let slm = Arc::new(SlmLane::new(
            llama_exe,
            grammar,
            cfg.slm_primary_gguf.clone(),
            cfg.effective_escalation_gguf().to_path_buf(),
            // SlmLane derives the escalation port as `port + 1`; a hand-edited
            // config that never went through Config::validate must not be able
            // to overflow that add.
            cfg.llama_port.min(u16::MAX - 1),
            cfg.slm_parallel,
        ));
        Pipeline::new(cfg, ledger, sidecar, slm, app)
    })
    .await
    .map_err(|e| e.to_string())
}

/// Upper bound on the one convertd probe an approval is allowed to wait for,
/// matching `preflight::run_with`'s liveness probe. Not the per-request
/// timeout: nothing in the review path converts a document.
///
/// The ceiling tracks preflight's deliberately — see the note there. A five
/// second bound made an approval fail on exactly the machines where a cold
/// convertd start is slowest, which is also where the reviewer most needs the
/// approval to go through.
fn review_probe_timeout(cfg: &Config) -> Duration {
    Duration::from_secs(cfg.sidecar_timeout_secs.clamp(1, 60))
}

#[tauri::command]
fn set_paused(state: tauri::State<AppState>, paused: bool) -> Result<(), String> {
    if let Some(pl) = state.pipeline.lock().unwrap().as_ref() {
        pl.paused
            .store(paused, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    } else {
        Err("BackLog is not running, so there is nothing to pause.".into())
    }
}

#[tauri::command]
async fn start_pipeline(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let cfg = healed_cfg(&state);
    cfg.validate()?;

    if state.pipeline.lock().unwrap().is_some() {
        return Ok(()); // already running
    }

    // Fail-closed gate: a machine that merely has a coherent config can
    // still be missing binaries, models, or writable folders. Run the live
    // check (and cache it for the Readiness panel) before ever spawning the
    // sidecars.
    let (running, paused) = runtime_flags(&state);
    let status = preflight::run(&app, &cfg, running, paused).await;
    *state.last_preflight.lock().unwrap() = Some(status.clone());
    if !status.configured {
        return Err(status.summary());
    }

    // Sweep orphaned cached document text past its TTL before starting. The
    // ledger-aware form is what keeps a flagged file's evidence pane alive
    // over a two-week holiday, so it must be the one the app calls.
    if !cfg.retain_cache {
        pipeline::sweep_cache_with_ledger(&cfg.cache_dir, cfg.cache_ttl_days, &state.ledger);
    }
    // The events table holds the most sensitive derived text of anything the
    // app keeps and had no retention policy at all.
    match state.ledger.sweep_events(cfg.cache_ttl_days.max(30)) {
        Ok(0) => {}
        Ok(removed) => log::info!("swept {removed} expired ledger events"),
        Err(error) => log::warn!("event sweep failed: {error}"),
    }

    let grammar = std::fs::read_to_string(resource(&app, "name.gbnf"))
        .map_err(|e| format!("grammar load failed: {e}"))?;
    let sidecar = Arc::new(
        Sidecar::with_timeout(
            binary(&app, "convertd")?,
            Duration::from_secs(cfg.sidecar_timeout_secs),
        )
        .with_models_dir(model_download::resolve_models_dir(&app))
        // Matches `convert_slots` below, so the semaphore that admits documents
        // to the convert stage and the number of processes that can serve them
        // are the same number. They were not before: one process served every
        // document while the semaphore cheerfully admitted six.
        .with_workers(cfg.convert_workers),
    );
    let slm = Arc::new(SlmLane::new(
        binary(&app, "llama-server")?,
        grammar,
        cfg.slm_primary_gguf.clone(),
        cfg.effective_escalation_gguf().to_path_buf(),
        cfg.llama_port,
        cfg.slm_parallel,
    ));
    let pipeline = Pipeline::new(cfg.clone(), state.ledger.clone(), sidecar, slm, app);

    let mut slot = state.pipeline.lock().unwrap();
    if slot.is_some() {
        return Ok(()); // a concurrent start_pipeline call won the race
    }
    watcher::spawn(pipeline.clone(), cfg.processing_dir.clone()).map_err(|e| e.to_string())?;
    *slot = Some(pipeline);
    drop(slot);

    if let Some(cached) = state.last_preflight.lock().unwrap().as_mut() {
        cached.running = true;
        cached.paused = false;
    }
    log::info!("pipeline started");
    Ok(())
}

/// Tear the pipeline down. **Only reachable from the exit path** — see the
/// `stop_pipeline` note below.
///
/// Ordering: pause so anything already past the ingest gate parks instead of
/// reaching for a sidecar that is about to disappear, close the ingest
/// semaphore so queued watcher tasks bail out rather than start new work, then
/// latch and kill the converter and model servers. Claims are process-local
/// ownership and must be released before Tauri's `std::process::exit` skips
/// every destructor.
fn stop_pipeline_inner(state: &AppState) {
    let taken = state.pipeline.lock().unwrap().take();
    let Some(pipeline) = taken else { return };
    pipeline
        .paused
        .store(true, std::sync::atomic::Ordering::Relaxed);
    pipeline.ingest_slots.close();
    pipeline.slm.begin_shutdown();
    let converter_failures = pipeline.sidecar.begin_shutdown();
    if converter_failures > 0 {
        log::error!(
            "{converter_failures} converter process(es) could not be terminated during shutdown"
        );
    }
    match pipeline.ledger.release_all_claims() {
        Ok(0) => {}
        Ok(released) => log::info!("released {released} active pipeline claims"),
        Err(error) => log::warn!("could not release active pipeline claims: {error}"),
    }
    // Deliberately NOT invalidating `last_preflight`: stopping says nothing
    // about whether this computer is still ready, and clearing it flipped
    // every row of the Readiness panel red with "Press Check again" — which,
    // for the user this appliance is built for, reads as "I broke it".
    // `get_runtime_status` already overlays the running/paused flags.
    if converter_failures == 0 {
        log::info!("pipeline stopped; converter and model servers terminated");
    } else {
        log::warn!("pipeline stopped; model server terminated, converter shutdown reported errors");
    }
}

// `stop_pipeline` is deliberately NOT a command yet.
//
// Stop is only honest once Start is genuinely reversible, and two pieces of
// that live outside this file: `watcher::spawn` parks a named thread on
// `for result in rx` forever with no way to signal it, and `Sidecar` kills its
// convertd child only from `Proc::drop`. Because the watcher thread holds the
// `Arc<Pipeline>` — and through it the `Arc<Sidecar>` — every Start/Stop/Start
// cycle would leak one thread, one directory watch, and one warm Python
// process on an appliance meant to run for weeks. A Stop button that leaks a
// convertd per press is worse than no Stop button, so the command stays
// unregistered until the watcher handle and `Sidecar::shutdown` requested of
// pipeline-integrity land. `stop_pipeline_inner` is still called on exit,
// where the process image is about to go away regardless and the only thing
// that matters is that no child outlives it.

/// Tear the pipeline down before the process goes away.
///
/// `App::run` exits through `std::process::exit`, which runs no destructors,
/// so the managed `AppState` holding `Arc<SlmLane>` / `Arc<Sidecar>` is never
/// dropped and neither `Drop` impl — both written specifically to prevent
/// orphaned children — ever fires on the one path that matters. Every quit
/// used to leak a llama-server with a multi-GB model resident, and the next
/// launch would then bind to that stale orphan because `ensure_up` trusts
/// anything answering `/health` on the fixed port.
fn shutdown_for_exit(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        stop_pipeline_inner(&state);
    }
    log::logger().flush();
}

/// How long to wait before the one retry that separates "the DPAPI key no
/// longer decrypts" from "the file was busy for a moment".
const LEDGER_RETRY_DELAY: Duration = Duration::from_millis(400);

/// Open the ledger; if it cannot be opened at all, move the unreadable
/// database (and its key) aside and start a fresh one so the app still
/// launches.
///
/// `ledger.rs` self-heals only a *missing* key. A profile restored from
/// backup, a re-imaged machine, or an admin password reset destroys the DPAPI
/// master key and leaves a key blob that decrypts to nothing — ordinary IT
/// events that otherwise turn into an app which simply never appears when
/// double-clicked, for a user who by design never opens a terminal.
fn open_ledger_with_recovery(db_path: &Path) -> Result<(Arc<Ledger>, Option<String>), String> {
    let first = match Ledger::open(db_path) {
        Ok(ledger) => return Ok((Arc::new(ledger), None)),
        Err(error) => error.to_string(),
    };
    // Starting fresh discards the dedup history of a multi-thousand-file
    // backfill and re-drives the whole batch, so it must never be the answer
    // to a transient failure — an antivirus scanner holding the file open,
    // AppData or OneDrive briefly unavailable, a lock left by an instance
    // that has not finished exiting. One retry costs half a second and rules
    // most of those out.
    std::thread::sleep(LEDGER_RETRY_DELAY);
    let second = match Ledger::open(db_path) {
        Ok(ledger) => {
            log::warn!("ledger opened on the second attempt; the first failed transiently");
            return Ok((Arc::new(ledger), None));
        }
        Err(error) => error.to_string(),
    };

    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    // The `-wal` of an SQLCipher database can hold a substantial tail of
    // committed but un-checkpointed transactions — precisely the most recent
    // processing history a support engineer would want, and the part the user
    // is being told was kept. It moves aside with the database rather than
    // being deleted; leaving it in place would make the fresh db unopenable in
    // exactly the same way.
    let db_name = db_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let aside_paths = [
        db_path.to_path_buf(),
        db_path.with_extension("key"),
        db_path.with_file_name(format!("{db_name}-wal")),
        db_path.with_file_name(format!("{db_name}-shm")),
    ];
    for path in aside_paths {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if path.exists() {
            let aside = path.with_file_name(format!("{name}.unreadable-{stamp}.bak"));
            let _ = std::fs::rename(&path, &aside);
        }
    }

    match Ledger::open(db_path) {
        Ok(ledger) => Ok((
            Arc::new(ledger),
            Some(format!(
                "BackLog could not read its record of previously processed files, so it started \
                 a fresh one. Nothing was deleted — the old files were kept next to it, ending \
                 in .unreadable-{stamp}.bak. Files already filed in SharePoint stay there; \
                 anything still in the Processing folder will be looked at again.\n\nTechnical \
                 detail: {first}"
            )),
        )),
        Err(third) => Err(format!(
            "BackLog could not open or recreate its record of processed files, so it cannot \
             start.\n\nTechnical detail: {first}\nOn retry: {second}\nAfter starting fresh: \
             {third}"
        )),
    }
}

/// Claim a per-user Windows mutex before opening the ledger.
///
/// Startup recovery clears claims left by the prior process, so allowing two
/// tray instances to initialize concurrently would make both believe they own
/// the same document. The raw handle intentionally remains open for the
/// lifetime of the process; Windows releases it automatically on exit.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SingleInstanceDecision {
    Acquired,
    AlreadyRunning,
    Failed,
}

#[cfg(any(windows, test))]
fn classify_single_instance(handle: isize, last_error: u32) -> SingleInstanceDecision {
    const ERROR_ALREADY_EXISTS: u32 = 183;
    if handle == 0 {
        SingleInstanceDecision::Failed
    } else if last_error == ERROR_ALREADY_EXISTS {
        SingleInstanceDecision::AlreadyRunning
    } else {
        SingleInstanceDecision::Acquired
    }
}

#[cfg(windows)]
fn show_single_instance_failure() {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(window: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    const MB_OK: u32 = 0;
    const MB_ICONERROR: u32 = 0x10;
    const MB_SETFOREGROUND: u32 = 0x0001_0000;
    let text: Vec<u16> = "BackLog could not establish its single-instance safety guard, so it \
                          will close without opening the processing ledger. Restart Windows or \
                          contact IT."
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let caption: Vec<u16> = "BackLog could not start"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: both strings are NUL-terminated and remain alive for the call;
    // a null owner is explicitly supported for a pre-window fatal dialog.
    let _ = unsafe {
        MessageBoxW(
            0,
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        )
    };
}

#[cfg(windows)]
fn claim_single_instance() -> bool {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(attributes: *mut c_void, initial_owner: i32, name: *const u16) -> isize;
        fn GetLastError() -> u32;
        fn CloseHandle(object: isize) -> i32;
    }

    let name: Vec<u16> = "Local\\ai.sonomos.backlog"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: attributes is null by contract, name is NUL-terminated and
    // remains alive for the call, and the returned handle is checked.
    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    // SAFETY: GetLastError is read immediately after CreateMutexW, before any
    // other Windows API call can replace the thread-local value.
    let last_error = unsafe { GetLastError() };
    match classify_single_instance(handle, last_error) {
        SingleInstanceDecision::Acquired => true,
        SingleInstanceDecision::AlreadyRunning => {
            // SAFETY: CreateMutexW returned a valid handle owned by this process.
            let _ = unsafe { CloseHandle(handle) };
            false
        }
        SingleInstanceDecision::Failed => {
            eprintln!("BackLog could not create its single-instance guard.");
            show_single_instance_failure();
            false
        }
    }
}

#[cfg(not(windows))]
fn claim_single_instance() -> bool {
    true
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if !claim_single_instance() {
        return;
    }
    let notice: Arc<Mutex<Option<StartupNotice>>> = Arc::new(Mutex::new(None));
    let setup_notice = notice.clone();

    let app = tauri::Builder::default()
        // Sidecars (convertd, llama-server) are spawned from Rust via
        // std::process::Command, so the webview needs neither the shell plugin
        // nor shell:allow-execute; the opener plugin is unused. Both removed to
        // shrink the IPC attack surface.
        .plugin(tauri_plugin_dialog::init())
        // Self-update: checks the `latest.json` endpoint configured under
        // `plugins.updater` in tauri.conf.json, verifies the release's
        // signature against the embedded pubkey, and (if the user accepts)
        // downloads + installs it. tauri-plugin-process supplies the
        // `relaunch()` the frontend calls after a successful install.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(move |app| {
            let version = app.package_info().version.to_string();
            let data_dir = match app.path().app_data_dir() {
                Ok(dir) => dir,
                Err(error) => {
                    // Nothing is logged yet and nothing can be: record the
                    // failure for the Ready handler and let the app come up
                    // far enough to say so.
                    *setup_notice.lock().unwrap() = Some(StartupNotice {
                        fatal: true,
                        message: format!(
                            "BackLog could not find the folder Windows gives it to store its \
                             settings, so it cannot start.\n\nTechnical detail: {error}"
                        ),
                    });
                    return Ok(());
                }
            };
            std::fs::create_dir_all(&data_dir).ok();
            let log_path = logging::init(&data_dir, &version);

            let cfg_path = data_dir.join("backlog.config.json");
            let default_cache_dir = data_dir.join("cache");
            let mut cfg = Config::load(&cfg_path);
            if cfg.cache_dir.as_os_str().is_empty() {
                cfg.cache_dir = default_cache_dir.clone();
            }
            // The shipped defaults (`Config::default()`) point at
            // "models/<file>", relative to whatever the process's current
            // directory happens to be — meaningless for an installed exe.
            // Rehome them under the persistent app-data models dir, which is
            // also where `download_models` (model_download.rs) writes and
            // where `BACKLOG_MODELS_DIR` points the convertd sidecar (below,
            // via `Sidecar::with_models_dir`). Absolute paths a user already
            // set via Settings' Browse dialog pass through untouched; see
            // `resolve_configured_model_path`'s doc comment.
            let models_dir = model_download::resolve_models_dir(app.handle());
            std::fs::create_dir_all(&models_dir).ok();
            // v0.4.4 persisted the primary path into both fields whenever the
            // optional model was missing. Migrate that collapsed value back to
            // a distinct desired destination so an in-app escalation download
            // becomes active on the next readiness check.
            if let Err(message) =
                repair_and_persist_startup_model_paths(&cfg_path, &mut cfg, &models_dir)
            {
                log::error!("model-path migration failed: {message}");
                *setup_notice.lock().unwrap() = Some(StartupNotice {
                    fatal: true,
                    message,
                });
                return Ok(());
            }
            // The installer ships the primary GGUF so a fresh machine can name
            // its first document without the 2.4 GB download.
            //
            // Relocate it into app-data rather than pointing the config at the
            // install directory. Keeping one canonical models dir is what makes
            // the rest of the system coherent: `download_models` writes to the
            // configured path, `BACKLOG_MODELS_DIR` hands that dir to convertd,
            // and preflight reports on it. A config pointing into the install
            // tree would send a later "Download models" *back into the install
            // tree*, and would be silently orphaned by the next upgrade.
            //
            // `rename` first because per-user installs land under the same
            // `%LOCALAPPDATA%`/`%APPDATA%` volume as app-data, making this
            // instant and free rather than a 639 MB copy. Copy is the
            // cross-volume fallback; if both fail the model simply is not there
            // and preflight says so, which is the same state as before.
            for (path, name) in [
                (&mut cfg.slm_primary_gguf, model_download::PRIMARY_GGUF_NAME),
                (
                    &mut cfg.slm_escalation_gguf,
                    model_download::ESCALATION_GGUF_NAME,
                ),
            ] {
                if path.is_file() {
                    continue;
                }
                let bundled = resource(app.handle(), &format!("models/{name}"));
                if !bundled.is_file() {
                    continue;
                }
                let dest = models_dir.join(name);
                if std::fs::rename(&bundled, &dest).is_ok() {
                    log::info!("installed the bundled {name} into the models folder");
                    *path = dest;
                } else if std::fs::copy(&bundled, &dest).is_ok() {
                    log::info!("copied the bundled {name} into the models folder");
                    *path = dest;
                } else {
                    log::warn!("could not place the bundled {name} into the models folder");
                }
            }
            for (target, expected_sha256) in [
                (
                    model_download::SEMANTIC_MODEL_TARGET,
                    model_download::SEMANTIC_MODEL_SHA256,
                ),
                (
                    model_download::SEMANTIC_VOCAB_TARGET,
                    model_download::SEMANTIC_VOCAB_SHA256,
                ),
            ] {
                let dest = models_dir.join(target);
                if dest.is_file()
                    && pipeline::hash_file(&dest)
                        .map(|actual| actual == expected_sha256)
                        .unwrap_or(false)
                {
                    continue;
                }
                let bundled = resource(app.handle(), &format!("models/{target}"));
                if !bundled.is_file() {
                    continue;
                }
                match pipeline::hash_file(&bundled) {
                    Ok(actual) if actual == expected_sha256 => {
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        if std::fs::copy(&bundled, &dest).is_ok() {
                            log::info!("copied bundled {target} into the models folder");
                        } else {
                            log::warn!("could not copy bundled {target} into the models folder");
                        }
                    }
                    Ok(actual) => log::warn!(
                        "bundled {target} has SHA-256 {actual}, expected {expected_sha256}; ignoring it"
                    ),
                    Err(error) => log::warn!("could not hash bundled {target}: {error}"),
                }
            }
            // After the bundled step, so a machine that received only the
            // primary still gets a usable escalation rung pointed at it.
            cfg.normalize();
            cfg.save(&cfg_path).ok();
            register_sensitive_paths(&cfg);

            let ledger = match open_ledger_with_recovery(&data_dir.join("ledger.db")) {
                Ok((ledger, recovered)) => {
                    if let Some(message) = recovered {
                        log::warn!("ledger recovered by starting fresh");
                        *setup_notice.lock().unwrap() = Some(StartupNotice {
                            fatal: false,
                            message,
                        });
                    }
                    ledger
                }
                Err(message) => {
                    log::error!("ledger unrecoverable: {message}");
                    *setup_notice.lock().unwrap() = Some(StartupNotice {
                        fatal: true,
                        message,
                    });
                    return Ok(());
                }
            };
            // Claims describe ownership by one running process, not durable
            // document state. A crash cannot run ClaimGuard::drop, so recover
            // those rows immediately instead of hiding them behind the stale
            // timeout on the next launch.
            match ledger.release_all_claims() {
                Ok(0) => {}
                Ok(released) => log::info!("recovered {released} interrupted pipeline claims"),
                Err(error) => log::warn!("could not recover interrupted claims: {error}"),
            }
            match pipeline::reconcile_terminal_manifests(&cfg, &ledger) {
                Ok(0) => {}
                Ok(recovered) => {
                    log::info!("reconciled {recovered} durable pipeline outcomes")
                }
                Err(error) => log::warn!("could not reconcile durable pipeline outcomes: {error}"),
            }

            app.manage(AppState {
                cfg_path,
                cfg: Mutex::new(cfg),
                default_cache_dir,
                log_path,
                ledger,
                pipeline: Mutex::new(None),
                last_preflight: Mutex::new(None),
            });

            // System-tray appliance: closing the window hides it so the
            // pipeline (and the convertd / llama-server sidecars) keep running
            // in the background; the app exits only via the tray's Quit item.
            let show_i = MenuItem::with_id(app, "show", "Show BackLog", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &quit_i])?;
            let mut tray = TrayIconBuilder::with_id("backlog-tray")
                .tooltip("BackLog")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => reveal_main(app),
                    "quit" => {
                        // Belt and braces with the RunEvent handler below:
                        // whichever runs first, the sidecars are down before
                        // the process image goes away.
                        shutdown_for_exit(app);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        reveal_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            if let Some(win) = app.get_webview_window("main") {
                let w = win.clone();
                win.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            set_config,
            get_runtime_status,
            run_preflight,
            create_missing_dir,
            list_jobs,
            count_jobs,
            list_flagged,
            count_flagged,
            get_flagged_job,
            list_flag_reasons,
            get_stats,
            get_events,
            get_evidence,
            get_diagnostics,
            open_logs_folder,
            reveal_quarantined,
            resubmit,
            dismiss,
            reprocess,
            set_paused,
            start_pipeline,
            model_download::download_models,
            // The webview is the only caller of these two, so leaving them out
            // of this list made the Cancel button on a 2.4 GB download a
            // no-op that reported an unknown-command error, and left the
            // progress panel unable to re-sync after a view switch.
            model_download::cancel_model_download,
            model_download::model_download_status
        ])
        .build(tauri::generate_context!());

    let app = match app {
        Ok(app) => app,
        Err(error) => {
            // Pre-window failure with no event loop and therefore no dialog;
            // the file logger is not up either, so stderr is all there is.
            eprintln!("BackLog could not start: {error}");
            std::process::exit(1);
        }
    };

    // Driving the loop ourselves (rather than `Builder::run`) is what makes
    // the exit path observable at all — see `shutdown_for_exit`.
    app.run(move |app, event| match event {
        RunEvent::Ready => {
            if let Some(notice) = notice.lock().unwrap().take() {
                show_startup_notice(app, notice);
            }
        }
        RunEvent::ExitRequested { .. } | RunEvent::Exit => shutdown_for_exit(app),
        _ => {}
    });
}

/// Surface a startup failure once the event loop exists, because that is the
/// first moment a native dialog can be shown at all.
fn show_startup_notice(app: &tauri::AppHandle, notice: StartupNotice) {
    let handle = app.clone();
    let fatal = notice.fatal;
    app.dialog()
        .message(notice.message)
        .title(if fatal {
            "BackLog cannot start"
        } else {
            "BackLog started fresh"
        })
        .kind(if fatal {
            MessageDialogKind::Error
        } else {
            MessageDialogKind::Warning
        })
        .show(move |_| {
            if fatal {
                handle.exit(1);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `AppState` over real folders and a real (encrypted) ledger, with no
    /// pipeline running — the state a user is in on the first launch, which is
    /// exactly where the review commands used to be unreachable.
    fn stopped_state(root: &Path) -> AppState {
        for sub in ["proc", "out", "quar", "cache", "logs"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let cfg = Config {
            processing_dir: root.join("proc"),
            outbox_dir: root.join("out"),
            quarantine_dir: root.join("quar"),
            cache_dir: root.join("cache"),
            ..Default::default()
        };
        AppState {
            cfg_path: root.join("backlog.config.json"),
            cfg: Mutex::new(cfg),
            default_cache_dir: root.join("cache"),
            log_path: root.join("logs").join("backlog.log"),
            ledger: Arc::new(Ledger::open(&root.join("ledger.db")).unwrap()),
            pipeline: Mutex::new(None),
            last_preflight: Mutex::new(None),
        }
    }

    /// A flagged job with its original sitting in quarantine, as `flag()`
    /// leaves it.
    fn flagged_job(state: &AppState, sha: &str, relpath: &str) -> PathBuf {
        let cfg = state.cfg.lock().unwrap().clone();
        let name = relpath.rsplit('/').next().unwrap().to_string();
        let quarantined = cfg.quarantine_dir.join(format!("{}__{name}", &sha[..12]));
        std::fs::write(&quarantined, b"scanned bytes").unwrap();
        state
            .ledger
            .ingest(
                sha,
                &cfg.processing_dir.join(relpath).to_string_lossy(),
                &name,
                relpath,
                "pdf",
            )
            .unwrap();
        state
            .ledger
            .update_fields(
                sha,
                &[
                    (
                        "flag_reason",
                        Some("UNREADABLE:all conversion attempts exhausted".into()),
                    ),
                    (
                        "quarantine_path",
                        Some(quarantined.to_string_lossy().into_owned()),
                    ),
                ],
            )
            .unwrap();
        state
            .ledger
            .set_state(sha, ledger::JobState::Flagged)
            .unwrap();
        quarantined
    }

    /// The pending delivery `Pipeline::flag` leaves in the Outbox, which is
    /// what `dismiss` has to supersede rather than sit alongside.
    fn flagged_manifest(
        sha: &str,
        manifest_id: &str,
        original_name: &str,
        relpath: &str,
    ) -> manifest::Manifest {
        manifest::Manifest {
            schema: manifest::MANIFEST_SCHEMA_VERSION,
            manifest_id: manifest_id.to_string(),
            sha256: sha.to_string(),
            status: "flagged".into(),
            original_name: original_name.to_string(),
            original_relpath: relpath.to_string(),
            new_filename: None,
            description: None,
            date: None,
            date_source: None,
            doc_type: None,
            language: None,
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: Some("UNREADABLE:all conversion attempts exhausted".into()),
            model_versions: serde_json::json!({ "slm": "qwen3-0.6b" }),
            processed_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// A release build must never fall back to a bare executable name: the OS
    /// would resolve it against %PATH%, and a quarantined convertd.exe would
    /// then be silently replaced by whatever a user-writable PATH entry
    /// provides.
    #[test]
    fn release_binary_resolution_never_returns_a_bare_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe_dir = dir.path().join("app");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let resource_candidate = dir.path().join("resources").join("convertd");

        let resolved =
            resolve_binary(Some(&exe_dir), &resource_candidate, "convertd", false).unwrap();

        assert!(
            resolved.is_absolute(),
            "release must resolve to a real location: {resolved:?}"
        );
        assert!(
            resolved.parent().is_some_and(|p| !p.as_os_str().is_empty()),
            "release must never return a bare name: {resolved:?}"
        );
        assert!(resolved.starts_with(&exe_dir));

        // The dev fallback is the only path that may return a bare name.
        assert_eq!(
            resolve_binary(Some(&exe_dir), &resource_candidate, "convertd", true).unwrap(),
            PathBuf::from("convertd")
        );
    }

    /// The failure mode the whole rule exists for: `current_exe()` failing is
    /// the *same* class of event as the antivirus quarantine that removes
    /// convertd.exe. An unknown exe dir used to collapse to `PathBuf::new()`,
    /// and `"".join("convertd.exe")` is a bare relative name — handed to
    /// `std::process::Command`, that is the %PATH% resolution this is meant to
    /// have removed.
    #[test]
    fn an_unknown_install_location_is_an_error_rather_than_a_bare_name() {
        let dir = tempfile::tempdir().unwrap();
        let resource_candidate = dir.path().join("resources").join("convertd");

        let error = resolve_binary(None, &resource_candidate, "convertd", false)
            .expect_err("release with no install location must not produce a path to spawn");
        assert!(error.contains("where it is installed"), "got: {error}");

        // Same input in a dev build is still allowed to fall back, and that is
        // the only configuration in which a bare name may ever appear.
        assert_eq!(
            resolve_binary(None, &resource_candidate, "convertd", true).unwrap(),
            PathBuf::from("convertd")
        );
    }

    #[test]
    fn binary_resolution_prefers_a_real_file_beside_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let exe_dir = dir.path().to_path_buf();
        let staged = exe_dir.join("convertd");
        std::fs::write(&staged, b"#!/bin/sh\n").unwrap();
        let resolved = resolve_binary(
            Some(&exe_dir),
            Path::new("/nowhere/convertd"),
            "convertd",
            true,
        )
        .unwrap();
        assert_eq!(resolved, staged);
    }

    #[test]
    fn set_config_rejects_a_nested_outbox_and_never_persists_it() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("backlog.config.json");
        let cache = dir.path().join("cache");

        let nested = Config {
            processing_dir: "/a/proc".into(),
            outbox_dir: "/a/proc/out".into(),
            quarantine_dir: "/a/quar".into(),
            cache_dir: cache.clone(),
            ..Default::default()
        };
        let error = apply_config(&cfg_path, &cache, nested).unwrap_err();
        assert!(error.contains("nested"), "got: {error}");
        assert!(
            !cfg_path.exists(),
            "an invalid config must never reach disk"
        );
    }

    /// The Browse dialog is not the only way a path arrives: Explorer's "Copy
    /// as path" includes quotes, and this is what the user pastes.
    #[test]
    fn a_quoted_space_padded_path_round_trips_through_set_config_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("backlog.config.json");
        let cache = dir.path().join("cache");

        let typed = Config {
            processing_dir: "  \"C:\\Intake\"  ".into(),
            outbox_dir: "'D:/Outbox' ".into(),
            quarantine_dir: " C:\\Quarantine".into(),
            cache_dir: PathBuf::new(),
            ..Default::default()
        };
        let saved = apply_config(&cfg_path, &cache, typed).unwrap();

        assert_eq!(saved.processing_dir, PathBuf::from("C:\\Intake"));
        assert_eq!(saved.outbox_dir, PathBuf::from("D:/Outbox"));
        assert_eq!(saved.quarantine_dir, PathBuf::from("C:\\Quarantine"));
        // An unset cache is app-managed, not an unfixable Blocked row.
        assert_eq!(saved.cache_dir, cache);

        // What get_config would hand back on the next launch.
        let reloaded = Config::load(&cfg_path);
        assert_eq!(reloaded.processing_dir, PathBuf::from("C:\\Intake"));
        assert_eq!(reloaded.cache_dir, cache);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saving_colliding_model_paths_persists_honest_degraded_readiness() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for sub in ["proc", "out", "quar", "cache", "models"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let shared_primary = root.join("models").join("shared.gguf");
        std::fs::write(&shared_primary, b"primary model").unwrap();
        let cfg_path = root.join("backlog.config.json");
        let cache = root.join("cache");
        let submitted = Config {
            processing_dir: root.join("proc"),
            outbox_dir: root.join("out"),
            quarantine_dir: root.join("quar"),
            cache_dir: cache.clone(),
            slm_primary_gguf: shared_primary.clone(),
            slm_escalation_gguf: shared_primary.clone(),
            ..Default::default()
        };

        let saved = apply_config(&cfg_path, &cache, submitted).unwrap();
        let expected_escalation = root
            .join("models")
            .join(model_download::ESCALATION_GGUF_NAME);
        assert_eq!(saved.slm_primary_gguf, shared_primary);
        assert_eq!(saved.slm_escalation_gguf, expected_escalation);

        let reloaded = Config::load(&cfg_path);
        assert_ne!(reloaded.slm_primary_gguf, reloaded.slm_escalation_gguf);
        assert_eq!(reloaded.slm_escalation_gguf, expected_escalation);
        assert_eq!(
            reloaded.effective_escalation_gguf(),
            shared_primary.as_path()
        );

        let paths = preflight::RuntimePaths {
            sidecar: root.join("convertd"),
            llama_server: root.join("llama-server"),
            grammar: root.join("name.gbnf"),
            models_dir: root.join("models"),
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

        let status = preflight::run_with(&paths, &reloaded, false, false).await;
        assert!(status.configured);
        assert!(status.primary_model_found);
        assert!(!status.escalation_model_found);
        assert!(status
            .problems
            .iter()
            .any(|problem| problem.code == "escalation_model_missing_using_primary"));
    }

    #[test]
    fn a_primary_at_the_canonical_escalation_path_is_rejected_not_persisted_twice() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let cfg_path = dir.path().join("backlog.config.json");
        let canonical_escalation = dir
            .path()
            .join("models")
            .join(model_download::ESCALATION_GGUF_NAME);
        let submitted = Config {
            processing_dir: dir.path().join("proc"),
            outbox_dir: dir.path().join("out"),
            quarantine_dir: dir.path().join("quar"),
            cache_dir: cache.clone(),
            slm_primary_gguf: canonical_escalation.clone(),
            slm_escalation_gguf: canonical_escalation,
            ..Default::default()
        };

        let error = apply_config(&cfg_path, &cache, submitted).unwrap_err();
        assert!(error.contains("must be different"), "got: {error}");
        assert!(
            !cfg_path.exists(),
            "an unresolved model collision must never reach disk"
        );
    }

    #[test]
    fn startup_collision_migration_persists_repaired_distinct_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("backlog.config.json");
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        let shared_primary = models_dir.join(model_download::PRIMARY_GGUF_NAME);
        let mut cfg = Config {
            slm_primary_gguf: shared_primary.clone(),
            slm_escalation_gguf: shared_primary.clone(),
            ..Default::default()
        };
        cfg.save(&cfg_path).unwrap();

        repair_and_persist_startup_model_paths(&cfg_path, &mut cfg, &models_dir).unwrap();

        let expected_escalation = models_dir.join(model_download::ESCALATION_GGUF_NAME);
        assert_eq!(cfg.slm_primary_gguf, shared_primary);
        assert_eq!(cfg.slm_escalation_gguf, expected_escalation);
        let reloaded = Config::load(&cfg_path);
        assert_eq!(reloaded.slm_primary_gguf, shared_primary);
        assert_eq!(reloaded.slm_escalation_gguf, expected_escalation);
        assert_ne!(reloaded.slm_primary_gguf, reloaded.slm_escalation_gguf);
    }

    #[test]
    fn windows_model_path_identity_ignores_case_and_separator_spelling() {
        assert!(windows_paths_equivalent(
            Path::new(r"C:\Users\Jane\AppData\Roaming\BackLog\models\Qwen3.gguf"),
            Path::new(r"c:/users/jane/appdata/roaming/backlog/models/qWEN3.GGUF")
        ));
        assert!(!windows_paths_equivalent(
            Path::new(r"C:\BackLog\models\primary.gguf"),
            Path::new(r"C:\BackLog\models\escalation.gguf")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn existing_model_aliases_are_treated_as_one_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("primary.gguf");
        let alias = dir.path().join("primary-alias.gguf");
        std::fs::write(&target, b"model").unwrap();
        symlink(&target, &alias).unwrap();

        assert!(model_paths_collide(&target, &alias));
    }

    #[test]
    fn evidence_ids_must_stay_hex() {
        assert!(is_ledger_key(&"a".repeat(64)));
        assert!(is_ledger_key("deadbeef-2"));
        assert!(!is_ledger_key(""));
        assert!(!is_ledger_key("../../../windows/system32/config/sam"));
        assert!(!is_ledger_key("..%2f..%2fsecret"));
        assert!(!is_ledger_key("dead beef"));
        assert!(!is_ledger_key(&"a".repeat(91)));
    }

    #[test]
    fn a_null_mutex_handle_fails_closed() {
        assert_eq!(
            classify_single_instance(0, 0),
            SingleInstanceDecision::Failed
        );
    }

    /// A DPAPI master key destroyed by a re-image leaves a key blob that
    /// decrypts to nothing. The app must still launch.
    #[test]
    fn an_undecryptable_ledger_key_is_moved_aside_and_a_fresh_ledger_opens() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.db");
        {
            let (ledger, recovered) = open_ledger_with_recovery(&db_path).unwrap();
            assert!(
                recovered.is_none(),
                "a clean first open must not report a recovery"
            );
            ledger
                .ingest(
                    "deadbeef",
                    "/x/secret.pdf",
                    "secret.pdf",
                    "secret.pdf",
                    "pdf",
                )
                .unwrap();
        }

        // Simulate the key blob no longer decrypting, and a write-ahead log
        // left behind by a session that did not checkpoint. For an SQLCipher
        // database that -wal holds the tail of committed transactions — the
        // most recent processing history, and exactly what a support engineer
        // would ask for.
        std::fs::write(db_path.with_extension("key"), b"not a valid protected blob").unwrap();
        let wal = db_path.with_file_name("ledger.db-wal");
        std::fs::write(&wal, b"uncheckpointed transactions").unwrap();

        let (ledger, recovered) = open_ledger_with_recovery(&db_path).unwrap();
        let message = recovered.expect("recovery must be reported to the user");
        assert!(message.contains(".unreadable-"), "got: {message}");
        assert!(
            ledger.get("deadbeef").unwrap().is_none(),
            "the fresh ledger must be empty, not a decrypt of the old one"
        );

        // The message says nothing was deleted, so nothing may be deleted.
        let kept: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".unreadable-"))
            .collect();
        assert!(
            kept.iter().any(|n| n.starts_with("ledger.db.unreadable-")),
            "the old db must be kept: {kept:?}"
        );
        assert!(
            kept.iter().any(|n| n.starts_with("ledger.key.unreadable-")),
            "the old key must be kept: {kept:?}"
        );
        assert!(
            kept.iter()
                .any(|n| n.starts_with("ledger.db-wal.unreadable-")),
            "the -wal must be moved aside, never removed: {kept:?}"
        );
        // The fresh ledger opens in WAL mode and so has a `-wal` of its own;
        // the invariant is that it is *that* file and not the old, unreadable
        // one, which would make the new db fail to open in the same way.
        if wal.exists() {
            assert_ne!(
                std::fs::read(&wal).unwrap(),
                b"uncheckpointed transactions",
                "the old -wal was left beside the new db"
            );
        }
    }

    /// A locked or briefly unavailable file must not cost the user the dedup
    /// history of a multi-thousand-file backfill. The retry is what separates
    /// "this key no longer decrypts" from "antivirus had the file open".
    #[test]
    fn a_ledger_that_opens_on_the_retry_is_not_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.db");
        let (ledger, _) = open_ledger_with_recovery(&db_path).unwrap();
        ledger
            .ingest("deadbeef", "/x/a.pdf", "a.pdf", "a.pdf", "pdf")
            .unwrap();
        drop(ledger);

        let (reopened, recovered) = open_ledger_with_recovery(&db_path).unwrap();
        assert!(recovered.is_none(), "a healthy ledger is never recovered");
        assert!(
            reopened.get("deadbeef").unwrap().is_some(),
            "the existing history must survive a reopen"
        );
        let aside = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".unreadable-"));
        assert!(!aside, "nothing may be moved aside on a successful open");
    }

    #[test]
    fn diagnostics_config_carries_no_folder_names() {
        let cfg = Config {
            processing_dir: "C:\\Users\\jane\\OneDrive\\2024 Terminations".into(),
            outbox_dir: "C:\\Users\\jane\\Outbox".into(),
            quarantine_dir: "C:\\Quarantine".into(),
            cache_dir: "C:\\Cache".into(),
            ..Default::default()
        };
        let redacted = redacted_config(&cfg).to_string();
        assert!(!redacted.contains("Terminations"), "got: {redacted}");
        assert!(!redacted.contains("jane"), "got: {redacted}");
        // The parts support actually needs are still there.
        assert!(redacted.contains("llama_port"));
        assert!(redacted.contains("Qwen3-0.6B-Q8_0.gguf"));
    }

    #[test]
    fn creatable_dir_fields_cover_every_configured_folder() {
        // `create_missing_dir` matches on these names; a field added to one
        // side and not the other would silently fall through to cache_dir.
        assert_eq!(preflight::CREATABLE_DIR_FIELDS.len(), 4);
        for field in preflight::CREATABLE_DIR_FIELDS {
            assert!(field.ends_with("_dir"), "unexpected field: {field}");
        }
    }

    #[test]
    fn create_missing_dir_creates_processing_and_refuses_anything_else() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        let target = dir.path().join("proc").join("nested-intake");
        state.cfg.lock().unwrap().processing_dir = target.clone();

        create_missing_dir_inner(&state, "processing_dir").unwrap();
        assert!(target.is_dir());
        // A repaired folder invalidates the cached readiness result.
        assert!(state.last_preflight.lock().unwrap().is_none());

        // The webview names a config field, never a path.
        assert!(create_missing_dir_inner(&state, "slm_primary_gguf").is_err());
        assert!(create_missing_dir_inner(&state, "../../etc").is_err());
    }

    #[test]
    fn an_unusable_cache_folder_self_heals_to_the_app_managed_one() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        // A file where the folder should be: the disk-full / locked-AppData
        // shape, which had no control anywhere in the UI to recover from.
        let blocked = dir.path().join("blocked-cache");
        std::fs::write(&blocked, b"not a folder").unwrap();
        state.cfg.lock().unwrap().cache_dir = blocked;

        let healed = healed_cfg(&state);

        assert_eq!(healed.cache_dir, state.default_cache_dir);
        assert_eq!(state.cfg.lock().unwrap().cache_dir, state.default_cache_dir);
        // Persisted, so the repair survives a restart.
        assert_eq!(
            Config::load(&state.cfg_path).cache_dir,
            state.default_cache_dir
        );
    }

    #[test]
    fn reveal_quarantined_uses_the_recorded_path_and_never_leaves_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        let sha = "a".repeat(64);
        let quarantined = flagged_job(&state, &sha, "sub/scan.pdf");

        assert_eq!(quarantined_path(&state, &sha).unwrap(), quarantined);

        // Nothing the webview sends can address a file outside quarantine.
        assert_eq!(
            quarantined_path(&state, "../../etc/passwd").unwrap_err(),
            "invalid id"
        );
        assert!(quarantined_path(&state, &"b".repeat(64)).is_err());

        // A path that has since escaped quarantine is refused, not revealed.
        state
            .ledger
            .update_fields(&sha, &[("quarantine_path", Some("/etc/passwd".into()))])
            .unwrap();
        assert!(quarantined_path(&state, &sha)
            .unwrap_err()
            .contains("no longer"));
    }

    /// The retry path a user reaches after installing a missing codec or
    /// unlocking a PDF: the original goes back to the *relative* location it
    /// came from, so its identity — and therefore its manifest id — is
    /// unchanged.
    #[test]
    fn reprocess_restores_the_original_into_its_subfolder_and_resets_the_job() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        let sha = "c".repeat(64);
        let quarantined = flagged_job(&state, &sha, "2024/terminations/scan.pdf");

        reprocess_inner(&state, &sha).unwrap();

        let restored = dir.path().join("proc").join("2024/terminations/scan.pdf");
        assert!(
            restored.is_file(),
            "the original must be back in Processing"
        );
        assert!(!quarantined.exists(), "and not left behind in quarantine");

        let job = state.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(job.state, ledger::JobState::Ingested);
        assert_eq!(job.attempts, 0);
        assert!(job.flag_reason.is_none());

        // The reason for the retry is on the record.
        let events = state.ledger.events_for(&sha, 10).unwrap();
        assert!(
            events.iter().any(|e| e.stage == "reprocess"),
            "got: {events:?}"
        );
    }

    #[test]
    fn reprocess_refuses_a_job_whose_original_has_gone() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        let sha = "d".repeat(64);
        let quarantined = flagged_job(&state, &sha, "scan.pdf");
        std::fs::remove_file(&quarantined).unwrap();

        assert!(reprocess_inner(&state, &sha).is_err());
        // Nothing moved, so the job keeps its diagnosis.
        assert_eq!(
            state.ledger.get(&sha).unwrap().unwrap().state,
            ledger::JobState::Flagged
        );
        assert!(reprocess_inner(&state, "not-hex-!").is_err());
    }

    #[test]
    fn a_dismissal_manifest_records_a_terminal_decision_without_a_filename() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        let sha = "e".repeat(64);
        flagged_job(&state, &sha, "sub/junk.pdf");
        let job = state.ledger.get(&sha).unwrap().unwrap();

        let m = dismissed_manifest(&job, " cover sheet, nothing to file ");

        assert_eq!(m.status, "dismissed");
        assert_eq!(m.sha256, sha);
        assert!(
            m.new_filename.is_none(),
            "a dismissed file is never renamed"
        );
        assert_eq!(
            m.flag_reason.as_deref(),
            Some("DISMISSED:cover sheet, nothing to file")
        );
        assert!(m.model_versions.is_object());
        // Same identity as the flagged manifest it supersedes: content bound
        // to the Processing-relative path recorded at ingest.
        assert_eq!(
            m.manifest_id,
            identity::instance_id(&sha, &identity::normalize_relpath("sub/junk.pdf"))
        );
        assert_eq!(
            dismissed_manifest(&job, "  ").flag_reason.as_deref(),
            Some("DISMISSED:no reason given")
        );
    }

    #[test]
    fn reprocess_refuses_a_terminal_job_without_moving_or_resetting_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        let cfg = state.cfg.lock().unwrap().clone();
        let sha = "e".repeat(64);
        let path = cfg.processing_dir.join("already-filed.pdf");
        std::fs::write(&path, b"already filed bytes").unwrap();
        state
            .ledger
            .ingest(
                &sha,
                &path.to_string_lossy(),
                "already-filed.pdf",
                "already-filed.pdf",
                "pdf",
            )
            .unwrap();
        assert!(state
            .ledger
            .set_state(&sha, ledger::JobState::Emitted)
            .unwrap());

        let error = reprocess_inner(&state, &sha).expect_err("terminal jobs are immutable");
        assert!(
            error.contains("Needs Review") || error.contains("Flagged"),
            "{error}"
        );
        assert!(path.exists(), "the terminal source must not be moved");
        assert_eq!(
            state.ledger.get(&sha).unwrap().unwrap().state,
            ledger::JobState::Emitted
        );
    }

    #[test]
    fn dismiss_refuses_an_unknown_or_malformed_id_before_touching_anything() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        assert_eq!(
            dismiss_inner(&state, "../../x", "junk").unwrap_err(),
            "invalid id"
        );
        assert!(dismiss_inner(&state, &"f".repeat(64), "junk").is_err());
        assert!(!dir.path().join("out").join("_manifests").exists());
    }

    /// Quit is reachable from the tray before Start has ever been pressed, and
    /// the exit handler runs on every exit regardless. Taking a pipeline that
    /// is not there must be a quiet no-op, not a panic on the way out.
    #[test]
    fn stopping_a_pipeline_that_never_started_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        *state.last_preflight.lock().unwrap() = Some(RuntimeStatus::unchecked(false, false));

        stop_pipeline_inner(&state);
        stop_pipeline_inner(&state);

        assert!(state.pipeline.lock().unwrap().is_none());
        // Nothing was torn down, so the cached readiness result still
        // describes this machine and is left alone.
        assert!(state.last_preflight.lock().unwrap().is_some());
    }

    #[test]
    fn a_state_filter_typo_is_an_error_rather_than_a_silently_unfiltered_list() {
        assert_eq!(parse_state_filter(None).unwrap(), None);
        assert_eq!(parse_state_filter(Some("  ")).unwrap(), None);
        assert_eq!(
            parse_state_filter(Some("flagged")).unwrap(),
            Some(ledger::JobState::Flagged)
        );
        assert!(parse_state_filter(Some("flaged")).is_err());
    }

    #[test]
    fn events_are_readable_per_job_and_only_by_a_well_formed_id() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        let sha = "1".repeat(64);
        flagged_job(&state, &sha, "scan.pdf");
        state
            .ledger
            .log_event(&sha, "convert", "attempt 1 failed: empty extraction")
            .unwrap();

        let events = get_events_inner(&state, &sha, None).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].detail, "attempt 1 failed: empty extraction");

        // The command's own guard, not the ledger's: every shape the webview
        // could send that is not a ledger key is refused before any query.
        for bad in ["../../x", "dead beef", "a".repeat(91).as_str()] {
            assert_eq!(
                get_events_inner(&state, bad, None).unwrap_err(),
                "invalid id",
                "accepted {bad:?}"
            );
        }
    }

    /// The events table is the largest thing in the ledger and the limit
    /// arrives from the webview, so an unbounded read is not on offer.
    #[test]
    fn the_event_limit_is_clamped_however_large_the_webview_asks_for() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        let sha = "2".repeat(64);
        flagged_job(&state, &sha, "scan.pdf");
        for i in 0..MAX_EVENTS_PER_CALL + 20 {
            state
                .ledger
                .log_event(&sha, "convert", &format!("attempt {i}"))
                .unwrap();
        }

        assert_eq!(
            get_events_inner(&state, &sha, Some(usize::MAX))
                .unwrap()
                .len(),
            MAX_EVENTS_PER_CALL
        );
        assert_eq!(get_events_inner(&state, &sha, None).unwrap().len(), 100);
        assert_eq!(get_events_inner(&state, &sha, Some(3)).unwrap().len(), 3);
    }

    /// The whole dismissal, in the shape the pipeline actually leaves behind:
    /// a flagged job, its flagged manifest pending in the Outbox, and the
    /// cached document text on disk.
    #[test]
    fn dismiss_replaces_the_pending_flagged_manifest_and_purges_the_cached_text() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        let sha = "3".repeat(64);
        flagged_job(&state, &sha, "sub/junk.pdf");
        let cfg = state.cfg.lock().unwrap().clone();
        let manifest_id = identity::instance_id(&sha, &identity::normalize_relpath("sub/junk.pdf"));
        let pending = flagged_manifest(&sha, &manifest_id, "junk.pdf", "sub/junk.pdf");
        manifest::write_manifest(&cfg.manifests_dir(), &pending).unwrap();
        let cached = cfg.cache_dir.join(format!("{sha}.md"));
        std::fs::write(&cached, "# harvested document text").unwrap();

        dismiss_inner(&state, &sha, "cover sheet").unwrap();

        // One delivery, superseded in place: Flow 2 must not see two.
        let written: manifest::Manifest = serde_json::from_slice(
            &std::fs::read(cfg.manifests_dir().join(format!("{manifest_id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(written.status, "dismissed");
        assert_eq!(written.manifest_id, manifest_id);
        assert_eq!(
            written.flag_reason.as_deref(),
            Some("DISMISSED:cover sheet")
        );
        assert!(written.new_filename.is_none());

        assert_eq!(
            state.ledger.get(&sha).unwrap().unwrap().state,
            ledger::JobState::Dismissed
        );
        let events = state.ledger.events_for(&sha, 10).unwrap();
        assert!(
            events.iter().any(|e| e.stage == "dismiss"),
            "the decision must be on the record: {events:?}"
        );
        assert!(
            !cached.exists(),
            "a dismissal is a decision not to keep this document"
        );
    }

    /// A stale review row, a double-click, or a retry must never be able to
    /// retire something that is not in the review queue — the ledger permits
    /// Ingested/Converting/Named -> Dismissed, so the guard has to be here.
    #[test]
    fn dismiss_fails_closed_on_a_job_that_is_not_in_the_review_queue() {
        let dir = tempfile::tempdir().unwrap();
        let state = stopped_state(dir.path());
        let cfg = state.cfg.lock().unwrap().clone();

        // Already filed: an 'ok' manifest is with Flow 2 and the row is
        // terminal.
        let emitted = "4".repeat(64);
        state
            .ledger
            .ingest(&emitted, "/p/filed.pdf", "filed.pdf", "filed.pdf", "pdf")
            .unwrap();
        let manifest_id =
            identity::instance_id(&emitted, &identity::normalize_relpath("filed.pdf"));
        let ok = manifest::Manifest {
            status: "ok".into(),
            new_filename: Some("2024-01-02 Filed Thing.pdf".into()),
            description: Some("A filed thing.".into()),
            date: Some("2024-01-02".into()),
            date_source: Some("document".into()),
            flag_reason: None,
            model_versions: serde_json::json!({ "slm": "qwen3-0.6b" }),
            ..flagged_manifest(&emitted, &manifest_id, "filed.pdf", "filed.pdf")
        };
        manifest::write_manifest(&cfg.manifests_dir(), &ok).unwrap();
        state
            .ledger
            .set_state(&emitted, ledger::JobState::Emitted)
            .unwrap();

        let error = dismiss_inner(&state, &emitted, "junk").unwrap_err();
        assert!(error.contains("Needs Review"), "got: {error}");

        let untouched: manifest::Manifest = serde_json::from_slice(
            &std::fs::read(cfg.manifests_dir().join(format!("{manifest_id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(untouched.status, "ok", "an emitted delivery is frozen");
        assert_eq!(
            state.ledger.get(&emitted).unwrap().unwrap().state,
            ledger::JobState::Emitted
        );

        // And a file a worker is still mid-pipeline on is equally off limits:
        // Converted is a live, non-terminal rung, not a review outcome.
        let in_flight = "5".repeat(64);
        state
            .ledger
            .ingest(&in_flight, "/p/live.pdf", "live.pdf", "live.pdf", "pdf")
            .unwrap();
        state
            .ledger
            .set_state(&in_flight, ledger::JobState::Converted)
            .unwrap();
        assert!(dismiss_inner(&state, &in_flight, "junk").is_err());
    }

    /// The Approve button's one realistic failure — convertd unreachable, so
    /// `model_versions` is `{}` and manifest v3 refuses the delivery — must not
    /// reach the webview as schema jargon.
    #[test]
    fn a_missing_model_provenance_is_reported_in_plain_language() {
        let provenance_error = manifest::Manifest {
            schema: manifest::MANIFEST_SCHEMA_VERSION,
            manifest_id: "a".repeat(64),
            sha256: "b".repeat(64),
            status: "ok".into(),
            original_name: "a.pdf".into(),
            original_relpath: "a.pdf".into(),
            new_filename: Some("2024-01-02 Thing.pdf".into()),
            description: Some("A thing.".into()),
            date: Some("2024-01-02".into()),
            date_source: Some("human".into()),
            doc_type: None,
            language: None,
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: None,
            model_versions: serde_json::json!({}),
            processed_at: chrono::Utc::now().to_rfc3339(),
        }
        .validate()
        .expect_err("manifest v3 must refuse an ok delivery with no provenance");

        let shown = friendly_resubmit_error(provenance_error);
        assert!(!shown.contains("model_versions"), "got: {shown}");
        assert!(shown.contains("reads documents"), "got: {shown}");

        // Anything else is passed through rather than mistranslated.
        assert_eq!(
            friendly_resubmit_error(anyhow::anyhow!("date is not in the document")),
            "date is not in the document"
        );
    }

    /// Review-time provenance probes use the same bounded cold-start allowance
    /// as Settings preflight: configured timeout, clamped to 1..=60 seconds.
    #[test]
    fn the_review_path_tracks_the_bounded_preflight_timeout() {
        assert_eq!(
            review_probe_timeout(&Config {
                sidecar_timeout_secs: 45,
                ..Default::default()
            }),
            Duration::from_secs(45)
        );
        assert_eq!(
            review_probe_timeout(&Config {
                sidecar_timeout_secs: 0,
                ..Default::default()
            }),
            Duration::from_secs(1)
        );
        assert_eq!(
            review_probe_timeout(&Config {
                sidecar_timeout_secs: 300,
                ..Default::default()
            }),
            Duration::from_secs(60)
        );
    }
}
