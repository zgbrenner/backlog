//! Folder watcher over the OneDrive-synced processing dir. Debounced, and
//! with a size-stability check because OneDrive writes files in visible
//! partial states; hashing a half-synced file wastes a job slot and creates
//! a phantom "duplicate" when the full file lands.

use crate::pipeline::Pipeline;
use notify_debouncer_full::{new_debouncer, notify::RecursiveMode, DebounceEventResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const IGNORED_PREFIXES: &[&str] = &["~$", ".", "_"];
const STABILITY_PROBES: u32 = 3;
const STABILITY_INTERVAL_MS: u64 = 700;

pub fn spawn(pipeline: Arc<Pipeline>, dir: PathBuf) -> anyhow::Result<()> {
    let rt = tokio::runtime::Handle::current();
    std::thread::Builder::new().name("backlog-watcher".into()).spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut debouncer = match new_debouncer(Duration::from_secs(2), None, tx) {
            Ok(d) => d,
            Err(e) => {
                log::error!("watcher init failed: {e}");
                return;
            }
        };
        if let Err(e) = debouncer.watch(&dir, RecursiveMode::Recursive) {
            log::error!("cannot watch {dir:?}: {e}");
            return;
        }
        log::info!("watching {dir:?}");

        // Sweep pre-existing + resumable files on startup.
        for entry in walk(&dir) {
            enqueue(&rt, &pipeline, entry);
        }

        for result in rx {
            match result {
                DebounceEventResult::Ok(events) => {
                    for ev in events {
                        for p in ev.paths.clone() {
                            if is_candidate(&p) {
                                enqueue(&rt, &pipeline, p);
                            }
                        }
                    }
                }
                DebounceEventResult::Err(errs) => {
                    for e in errs {
                        log::warn!("watch error: {e}");
                    }
                }
            }
        }
    })?;
    Ok(())
}

fn enqueue(rt: &tokio::runtime::Handle, pipeline: &Arc<Pipeline>, path: PathBuf) {
    let pl = pipeline.clone();
    rt.spawn(async move {
        // Global backpressure: bound how many files are stability-probed and
        // hashed at once. A 3,000-file backfill still enqueues 3,000 cheap
        // parked tasks, but only a bounded few do blocking work concurrently.
        let _permit = match pl.ingest_slots.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return, // semaphore closed on shutdown
        };
        if !wait_stable(&path).await {
            return;
        }
        pl.process_file(path).await;
    });
}

fn is_candidate(p: &Path) -> bool {
    if !p.is_file() {
        return false;
    }
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    !IGNORED_PREFIXES.iter().any(|pre| name.starts_with(pre))
}

/// A file is "stable" when its size stops changing across probes and it can
/// be opened for read. Half-synced OneDrive files fail one of the two.
async fn wait_stable(path: &Path) -> bool {
    let mut last: Option<u64> = None;
    for _ in 0..STABILITY_PROBES * 10 {
        let size = match std::fs::metadata(path) {
            Ok(m) => m.len(),
            Err(_) => return false, // vanished (moved by us or by sync)
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
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if is_candidate(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}
