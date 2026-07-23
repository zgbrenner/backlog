mod config;
mod filter;
mod identity;
mod ledger;
mod manifest;
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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WindowEvent};

struct AppState {
    cfg_path: PathBuf,
    cfg: Mutex<Config>,
    ledger: Arc<Ledger>,
    pipeline: Mutex<Option<Arc<Pipeline>>>,
    last_preflight: Mutex<Option<RuntimeStatus>>,
}

/// Current pipeline running/paused state, read fresh from `AppState` on every
/// call so cached preflight results can be overlaid with it without going
/// stale between an explicit `run_preflight` and a later `set_paused`.
fn runtime_flags(state: &AppState) -> (bool, bool) {
    let pipeline = state.pipeline.lock().unwrap();
    match pipeline.as_ref() {
        Some(pipeline) => (true, pipeline.paused.load(std::sync::atomic::Ordering::Relaxed)),
        None => (false, false),
    }
}

pub(crate) fn resource(app: &tauri::AppHandle, rel: &str) -> PathBuf {
    app.path()
        .resolve(format!("resources/{rel}"), tauri::path::BaseDirectory::Resource)
        .unwrap_or_else(|_| PathBuf::from(format!("resources/{rel}")))
}

fn reveal_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

pub(crate) fn binary(app: &tauri::AppHandle, name: &str) -> PathBuf {
    // Tauri externalBin sidecars sit next to the app binary with a target
    // triple suffix in dev; resolve both layouts.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let candidates = [
        exe_dir.join(name),
        exe_dir.join(format!("{name}.exe")),
        resource(app, name).with_file_name(name),
    ];
    for c in &candidates {
        if c.exists() {
            return c.clone();
        }
    }
    PathBuf::from(name) // PATH fallback for dev
}

#[tauri::command]
fn get_config(state: tauri::State<AppState>) -> Config {
    state.cfg.lock().unwrap().clone()
}

#[tauri::command]
fn set_config(state: tauri::State<AppState>, cfg: Config) -> Result<(), String> {
    cfg.save(&state.cfg_path).map_err(|e| e.to_string())?;
    *state.cfg.lock().unwrap() = cfg;
    // Settings changed underneath it; the last preflight result no longer
    // describes the current configuration, so drop it back to fail-closed
    // "unchecked" rather than let a stale pass linger in the UI.
    *state.last_preflight.lock().unwrap() = None;
    Ok(())
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
async fn run_preflight(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<RuntimeStatus, String> {
    let cfg = state.cfg.lock().unwrap().clone();
    let (running, paused) = runtime_flags(&state);
    let status = preflight::run(&app, &cfg, running, paused).await;
    *state.last_preflight.lock().unwrap() = Some(status.clone());
    Ok(status)
}

#[tauri::command]
fn list_jobs(state: tauri::State<AppState>, limit: Option<usize>) -> Result<Vec<ledger::Job>, String> {
    state.ledger.list_jobs(limit.unwrap_or(500)).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_flagged(state: tauri::State<AppState>) -> Result<Vec<ledger::Job>, String> {
    state.ledger.list_by_state(ledger::JobState::Flagged).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_stats(state: tauri::State<AppState>) -> Result<serde_json::Value, String> {
    state.ledger.stats().map_err(|e| e.to_string())
}

#[tauri::command]
fn get_evidence(state: tauri::State<AppState>, sha256: String) -> Result<String, String> {
    // The id is a ledger key (a content hash, optionally with a duplicate
    // suffix). Reject anything that isn't hex/'-' so a crafted value can't
    // traverse out of the cache dir through the `{sha256}.md` join.
    if sha256.is_empty()
        || sha256.len() > 90
        || !sha256.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return Err("invalid id".into());
    }
    let cfg = state.cfg.lock().unwrap().clone();
    let p = cfg.cache_dir.join(format!("{sha256}.md"));
    std::fs::read_to_string(p).map_err(|e| e.to_string())
}

#[tauri::command]
async fn resubmit(
    state: tauri::State<'_, AppState>,
    sha256: String,
    date: String,
    subject: String,
    description: String,
) -> Result<(), String> {
    let pl = state
        .pipeline
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "pipeline not started".to_string())?;
    pl.resubmit(&sha256, date, subject, description)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_paused(state: tauri::State<AppState>, paused: bool) -> Result<(), String> {
    if let Some(pl) = state.pipeline.lock().unwrap().as_ref() {
        pl.paused.store(paused, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    } else {
        Err("pipeline not started".into())
    }
}

#[tauri::command]
async fn start_pipeline(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let cfg = state.cfg.lock().unwrap().clone();
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

    // Sweep orphaned cached document text past its TTL before starting.
    if !cfg.retain_cache {
        pipeline::sweep_cache(&cfg.cache_dir, cfg.cache_ttl_days);
    }

    let grammar = std::fs::read_to_string(resource(&app, "name.gbnf"))
        .map_err(|e| format!("grammar load failed: {e}"))?;
    let sidecar = Arc::new(Sidecar::with_timeout(
        binary(&app, "convertd"),
        std::time::Duration::from_secs(cfg.sidecar_timeout_secs),
    ));
    let slm = Arc::new(SlmLane::new(
        binary(&app, "llama-server"),
        grammar,
        cfg.slm_primary_gguf.clone(),
        cfg.slm_escalation_gguf.clone(),
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
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::init();
    tauri::Builder::default()
        // Sidecars (convertd, llama-server) are spawned from Rust via
        // std::process::Command, so the webview needs neither the shell plugin
        // nor shell:allow-execute; the opener plugin is unused. Both removed to
        // shrink the IPC attack surface.
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir().expect("app data dir");
            std::fs::create_dir_all(&data_dir).ok();
            let cfg_path = data_dir.join("backlog.config.json");
            let mut cfg = Config::load(&cfg_path);
            if cfg.cache_dir.as_os_str().is_empty() {
                cfg.cache_dir = data_dir.join("cache");
            }
            let ledger = Arc::new(Ledger::open(&data_dir.join("ledger.db"))?);
            app.manage(AppState {
                cfg_path,
                cfg: Mutex::new(cfg),
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
                    "quit" => app.exit(0),
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
            list_jobs,
            list_flagged,
            get_stats,
            get_evidence,
            resubmit,
            set_paused,
            start_pipeline
        ])
        .run(tauri::generate_context!())
        .expect("error while running BackLog");
}
