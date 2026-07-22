//! Folder watcher over the OneDrive-synced processing dir. Debounced, and
//! with a size-stability check because OneDrive writes files in visible
//! partial states; hashing a half-synced file wastes a job slot and creates
//! a phantom duplicate when the full file lands.

use crate::pipeline::Pipeline;
use notify_debouncer_full::{new_debouncer, notify::RecursiveMode, DebounceEventResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// Flow 1 prefixes stable physical-delivery identifiers with `__bl_`, so a
// blanket underscore exclusion would silently discard every automated intake.
const IGNORED_PREFIXES: &[&str] = &["~$", "."];
const STABILITY_PROBES: u32 = 3;
const ZERO_BYTE_STABILITY_PROBES: u32 = 15;
const STABILITY_INTERVAL_MS: u64 = 700;
const MAX_STABILITY_PROBES: u32 = ZERO_BYTE_STABILITY_PROBES * 2;

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

fn required_stable_probes(size: u64) -> u32 {
    if size == 0 {
        // A transient zero-byte OneDrive placeholder receives a much longer
        // grace period, but a genuinely empty file eventually reaches routing
        // and is deterministically flagged as CORRUPT instead of disappearing.
        ZERO_BYTE_STABILITY_PROBES
    } else {
        STABILITY_PROBES
    }
}

/// A file is stable when its size remains unchanged across enough probes and
/// it can be opened for reading. Half-synced OneDrive files fail one of those
/// checks. Zero-byte files deliberately require a longer settling period.
async fn wait_stable(path: &Path) -> bool {
    let mut last_size = None;
    let mut unchanged_probes = 0u32;

    for _ in 0..MAX_STABILITY_PROBES {
        let size = match std::fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(_) => return false, // Vanished or was moved by a completed run.
        };

        if last_size == Some(size) {
            unchanged_probes += 1;
        } else {
            last_size = Some(size);
            unchanged_probes = 1;
        }

        if unchanged_probes >= required_stable_probes(size)
            && std::fs::File::open(path).is_ok()
        {
            return true;
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_flow_one_stable_delivery_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("__bl_5f2b6a9c__Shareholder Register.pdf");
        std::fs::write(&path, b"fixture").unwrap();

        assert!(is_candidate(&path));
    }

    #[test]
    fn ignores_hidden_and_office_lock_files() {
        let dir = tempfile::tempdir().unwrap();
        let hidden = dir.path().join(".partial.pdf");
        let office_lock = dir.path().join("~$Agreement.docx");
        std::fs::write(&hidden, b"fixture").unwrap();
        std::fs::write(&office_lock, b"fixture").unwrap();

        assert!(!is_candidate(&hidden));
        assert!(!is_candidate(&office_lock));
    }

    #[test]
    fn zero_byte_files_receive_longer_settling_period() {
        assert_eq!(required_stable_probes(0), ZERO_BYTE_STABILITY_PROBES);
        assert_eq!(required_stable_probes(1), STABILITY_PROBES);
    }
}
