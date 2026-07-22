mod checker;
mod config;
mod filter;
mod harvest;
mod identity;
mod ledger;
mod manifest;
mod pipeline;
mod preflight;
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
use preflight::RuntimeStatus;
use sidecar::Sidecar;
use slm::SlmLane;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct AppState {
    cfg_path: PathBuf,
    cfg: Mutex<Config>,
    ledger: Arc<Ledger>,
    pipeline: Mutex<Option<Arc<Pipeline>>>,
    last_preflight: Mutex<Option<RuntimeStatus>>,
}

fn runtime_flags(state: &AppState) -> (bool, bool) {
    let pipeline = state.pipeline.lock().unwrap();
    match pipeline.as_ref() {
        Some(pipeline) => (true, pipeline.is_paused()),
        None => (false, false),
    }
}

#[tauri::command]
fn get_config(state: tauri::State<AppState>) -> Config {
    state.cfg.lock().unwrap().clone()
}

#[tauri::command]
fn set_config(state: tauri::State<AppState>, cfg: Config) -> Result<(), String> {
    if state.pipeline.lock().unwrap().is_some() {
        return Err(
            "Settings cannot change while the pipeline is running. Restart BackLog before changing runtime paths or models."
                .into(),
        );
    }
    cfg.save(&state.cfg_path)
        .map_err(|error| error.to_string())?;
    *state.cfg.lock().unwrap() = cfg;
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
async fn run_preflight(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> RuntimeStatus {
    let cfg = state.cfg.lock().unwrap().clone();
    let (running, paused) = runtime_flags(&state);
    let status = preflight::run(&app, &cfg, running, paused).await;
    *state.last_preflight.lock().unwrap() = Some(status.clone());
    status
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
    let pipeline = state
        .pipeline
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "pipeline not started".to_string())?;
    pipeline.set_paused(paused);

    if let Some(status) = state.last_preflight.lock().unwrap().as_mut() {
        status.running = true;
        status.paused = paused;
    }
    Ok(())
}

#[tauri::command]
async fn start_pipeline(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    if state.pipeline.lock().unwrap().is_some() {
        return Ok(());
    }

    let cfg = state.cfg.lock().unwrap().clone();
    let status = preflight::run(&app, &cfg, false, false).await;
    *state.last_preflight.lock().unwrap() = Some(status.clone());
    if !status.configured {
        return Err(status.summary());
    }

    let grammar_path = preflight::resolve_resource(&app, "name.gbnf");
    let grammar = std::fs::read_to_string(&grammar_path)
        .map_err(|error| format!("grammar load failed at {}: {error}", grammar_path.display()))?;
    let sidecar_executable = preflight::resolve_binary(&app, "convertd")
        .ok_or_else(|| "convertd sidecar disappeared after preflight".to_string())?;
    let llama_executable = preflight::resolve_binary(&app, "llama-server")
        .ok_or_else(|| "llama-server disappeared after preflight".to_string())?;

    let sidecar = Arc::new(Sidecar::with_timeout(
        sidecar_executable,
        Duration::from_secs(cfg.sidecar_timeout_secs),
    ));
    let slm = Arc::new(SlmLane::new(
        llama_executable,
        grammar,
        cfg.slm_primary_gguf.clone(),
        cfg.slm_escalation_gguf.clone(),
        cfg.llama_port,
        cfg.slm_parallel,
    ));
    let pipeline = Pipeline::new(
        cfg.clone(),
        state.ledger.clone(),
        sidecar,
        slm,
        app,
    );

    let mut slot = state.pipeline.lock().unwrap();
    if slot.is_some() {
        return Ok(());
    }
    watcher::spawn(pipeline.clone(), cfg.processing_dir.clone())
        .map_err(|error| error.to_string())?;
    *slot = Some(pipeline);
    drop(slot);

    if let Some(status) = state.last_preflight.lock().unwrap().as_mut() {
        status.running = true;
        status.paused = false;
    }
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
                last_preflight: Mutex::new(None),
            });
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
