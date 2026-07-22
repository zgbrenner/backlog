//! Folder watcher over the OneDrive-synced processing dir. Debounced, and
//! with a size-stability check because OneDrive writes files in visible
//! partial states; hashing a half-synced file wastes a job slot and creates
//! a phantom duplicate when the full file lands.

use crate::pipeline::Pipeline;
use notify_debouncer_full::{new_debouncer, notify::RecursiveMode, DebounceEventResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const IGNORED_PREFIXES: &[&str] = &["~$", ".", "_"];
const STABILITY_PROBES: u32 = 3;
const STABILITY_INTERVAL_MS: u64 = 700;

pub fn spawn(pipeline: Arc<Pipeline>, dir: PathBuf) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("backlog-watcher".into())
        .spawn(move || {
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut debouncer = match new_debouncer(Duration::from_secs(2), None, sender) {
                Ok(debouncer) => debouncer,
                Err(error) => {
                    log::error!("watcher init failed: {error}");
                    return;
                }
            };
            if let Err(error) = debouncer.watch(&dir, RecursiveMode::Recursive) {
                log::error!("cannot watch {dir:?}: {error}");
                return;
            }
            log::info!("watching {dir:?}");

            // Sweep every existing file on startup. Stable instance identity
            // makes this safe after a crash or a missed OneDrive event.
            for entry in walk(&dir) {
                enqueue(&runtime, &pipeline, entry);
            }

            for result in receiver {
                match result {
                    DebounceEventResult::Ok(events) => {
                        for event in events {
                            for path in event.paths {
                                if is_candidate(&path) {
                                    enqueue(&runtime, &pipeline, path);
                                }
                            }
                        }
                    }
                    DebounceEventResult::Err(errors) => {
                        for error in errors {
                            log::warn!("watch error: {error}");
                        }
                    }
                }
            }
        })?;
    Ok(())
}

fn enqueue(runtime: &tokio::runtime::Handle, pipeline: &Arc<Pipeline>, path: PathBuf) {
    let pipeline = pipeline.clone();
    runtime.spawn(async move {
        if !wait_stable(&path).await {
            return;
        }
        pipeline.process_file_recoverable(path).await;
    });
}

fn is_candidate(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    !IGNORED_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// A file is stable when its size stops changing across probes and it can be
/// opened for read. Half-synced OneDrive files fail one of the two checks.
async fn wait_stable(path: &Path) -> bool {
    let mut last = None;
    for _ in 0..STABILITY_PROBES * 10 {
        let size = match std::fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(_) => return false, // Vanished or was moved by a completed run.
        };
        if last == Some(size) && std::fs::File::open(path).is_ok() && size > 0 {
            return true;
        }
        last = Some(size);
        tokio::time::sleep(Duration::from_millis(STABILITY_INTERVAL_MS)).await;
    }
    log::warn!("file never stabilized: {path:?}");
    false
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(directory) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if is_candidate(&path) {
                    files.push(path);
                }
            }
        }
    }
    files
}
