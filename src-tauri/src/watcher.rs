//! Folder watcher over the OneDrive-synced processing dir. Debounced, and
//! with a size-stability check because OneDrive writes files in visible
//! partial states; hashing a half-synced file wastes a job slot and creates
//! a phantom "duplicate" when the full file lands.

use crate::pipeline::Pipeline;
use notify_debouncer_full::{new_debouncer, notify::RecursiveMode, DebounceEventResult};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Office lock files (`~$doc.docx`) and dotfiles. A leading underscore used to
/// be here too, which silently dropped real documents like
/// `_DRAFT Agreement.docx` — no ledger row, no manifest, no log line — and
/// rejected the `__incoming_<token>__` intake envelope FLOW1-intake.md
/// instructs the builder to create, so a pilot following the docs saw total
/// silent failure.
const IGNORED_PREFIXES: &[&str] = &["~$", "."];
const STABILITY_PROBES: u32 = 3;
const STABILITY_INTERVAL_MS: u64 = 700;

pub fn spawn(pipeline: Arc<Pipeline>, dir: PathBuf) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("backlog-watcher".into())
        .spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut debouncer = match new_debouncer(Duration::from_secs(2), None, tx) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("watcher init failed: {e}");
                    return;
                }
            };

            // Sweep pre-existing + resumable files BEFORE arming the watcher: a
            // file that is both walked and evented would otherwise be driven
            // through the pipeline twice, concurrently.
            let swept: HashSet<PathBuf> = walk(&dir).into_iter().collect();
            for entry in &swept {
                enqueue(&pipeline, entry.clone());
            }

            if let Err(e) = debouncer.watch(&dir, RecursiveMode::Recursive) {
                log::error!("cannot watch {dir:?}: {e}");
                return;
            }
            log::info!("watching {dir:?}");

            // A file that landed *during* the walk is in neither the sweep nor the
            // event stream. This second pass is a directory listing, not a rehash:
            // only paths the sweep never saw are enqueued.
            for entry in walk(&dir) {
                if !swept.contains(&entry) {
                    enqueue(&pipeline, entry);
                }
            }

            for result in rx {
                match result {
                    DebounceEventResult::Ok(events) => {
                        for ev in events {
                            for p in ev.paths.clone() {
                                if is_candidate(&p) {
                                    enqueue(&pipeline, p);
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

fn enqueue(pipeline: &Arc<Pipeline>, path: PathBuf) {
    // Cheap duplicate gate. The ledger claim is the durable one, but reaching
    // it costs a full SHA-256 of the file; a second event for a path already
    // in flight is dropped here before any I/O. Held for the whole task,
    // released by the guard's Drop on every exit path.
    let Some(reserved) = pipeline.begin_path(&path) else {
        log::debug!("{path:?} is already in flight; dropping duplicate enqueue");
        return;
    };
    let pl = pipeline.clone();
    // tauri::async_runtime is reachable from any thread, so this can't panic
    // the way tokio's Handle::current() would if the caller (the sync
    // start_pipeline command) isn't running inside the runtime.
    tauri::async_runtime::spawn(async move {
        // Moved in so the reservation lives as long as the task does; its Drop
        // is what frees the path again.
        let _reserved = reserved;
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
    if IGNORED_PREFIXES.iter().any(|pre| name.starts_with(pre)) {
        // A skip that leaves no trace is indistinguishable from a pipeline
        // that never saw the file, which is the hardest kind of silence to
        // diagnose for a user who cannot open a terminal.
        log::info!("ignoring {p:?}: filename starts with a reserved prefix");
        return false;
    }
    true
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
