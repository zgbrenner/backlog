mod checker;
mod config;
mod filter;
mod harvest;
mod identity;
mod ledger;
mod manifest;
mod pipeline;
mod recovery;
mod routing;
mod sidecar;
mod slm;
mod watcher;

#[cfg(test)]
mod task3_tests;

use config::Config;
use ledger::Ledger;
use pipeline::Pipeline;
use sidecar::Sidecar;
use slm::SlmLane;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;

struct AppState {
    cfg_path: PathBuf,
    cfg: Mutex<Config>,
    ledger: Arc<Ledger>,
    pipeline: Mutex<Option<Arc<Pipeline>>>,
}

fn resource(app: &tauri::AppHandle, rel: &str) -> PathBuf {
    app.path()
        .resolve(
            format!("resources/{rel}"),
            tauri::path::BaseDirectory::Resource,
        )
        .unwrap_or_else(|_| PathBuf::from(format!("resources/{rel}")))
}

fn binary(app: &tauri::AppHandle, name: &str) -> PathBuf {
    // Tauri externalBin sidecars sit next to the app binary with a target
    // triple suffix in dev; resolve both layouts.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.to_path_buf()))
        .unwrap_or_default();
    let candidates = [
        exe_dir.join(name),
        exe_dir.join(format!("{name}.exe")),
        resource(app, name).with_file_name(name.to_string()),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return candidate.clone();
        }
    }
    PathBuf::from(name) // PATH fallback for development.
}

#[tauri::command]
fn get_config(state: tauri::State<AppState>) -> Config {
    state.cfg.lock().unwrap().clone()
}

#[tauri::command]
fn set_config(state: tauri::State<AppState>, cfg: Config) -> Result<(), String> {
    cfg.save(&state.cfg_path).map_err(|error| error.to_string())?;
    *state.cfg.lock().unwrap() = cfg;
    Ok(())
}

#[tauri::command]
fn list_jobs(
    state: tauri::State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<ledger::Job>, String> {
    state
        .ledger
        .list_jobs(limit.unwrap_or(500))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn list_flagged(state: tauri::State<AppState>) -> Result<Vec<ledger::Job>, String> {
    state
        .ledger
        .list_by_state(ledger::JobState::Flagged)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn get_stats(state: tauri::State<AppState>) -> Result<serde_json::Value, String> {
    state.ledger.stats().map_err(|error| error.to_string())
}

#[tauri::command]
fn get_evidence(state: tauri::State<AppState>, sha256: String) -> Result<String, String> {
    let cfg = state.cfg.lock().unwrap().clone();
    let path = cfg.cache_dir.join(format!("{sha256}.md"));
    std::fs::read_to_string(path).map_err(|error| error.to_string())
}

#[tauri::command]
async fn resubmit(
    state: tauri::State<'_, AppState>,
    sha256: String,
    date: String,
    subject: String,
    description: String,
) -> Result<(), String> {
    let pipeline = state
        .pipeline
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "pipeline not started".to_string())?;
    pipeline
        .resubmit(&sha256, date, subject, description)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn set_paused(state: tauri::State<AppState>, paused: bool) -> Result<(), String> {
    if let Some(pipeline) = state.pipeline.lock().unwrap().as_ref() {
        pipeline.set_paused(paused);
        Ok(())
    } else {
        Err("pipeline not started".into())
    }
}

#[tauri::command]
fn start_pipeline(app: tauri::AppHandle, state: tauri::State<AppState>) -> Result<(), String> {
    let cfg = state.cfg.lock().unwrap().clone();
    cfg.validate().map_err(|error| error.to_string())?;

    let mut slot = state.pipeline.lock().unwrap();
    if slot.is_some() {
        return Ok(()); // Already running.
    }

    let grammar = std::fs::read_to_string(resource(&app, "name.gbnf"))
        .map_err(|error| format!("grammar load failed: {error}"))?;
    let sidecar = Arc::new(Sidecar::with_timeout(
        binary(&app, "convertd"),
        Duration::from_secs(cfg.sidecar_timeout_secs),
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
    watcher::spawn(pipeline.clone(), cfg.processing_dir.clone())
        .map_err(|error| error.to_string())?;
    *slot = Some(pipeline);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::init();
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
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
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            set_config,
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
