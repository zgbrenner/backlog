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
                                if is_safe_path_under_root(&dir, &p) && is_candidate(&p) {
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
    if is_reparse_point(p) {
        log::warn!("ignoring reparse-point path outside the configured root: {p:?}");
        return false;
    }
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

/// Symlinks and Windows junctions are not documents. Checking metadata without
/// following links is important: `Path::is_file` and `Path::is_dir` both follow
/// them, which can make a recursive watcher ingest an unrelated tree.
fn is_reparse_point(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return true;
    };
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

/// Validate a path at every watcher boundary, not only when the notify event
/// arrives. A sync provider can replace a regular file with a symlink/junction
/// between the event and the worker; rechecking the canonical location and all
/// existing path components makes the worker fail closed instead of hashing or
/// quarantining a path that escaped Processing.
pub(crate) fn is_safe_path_under_root(root: &Path, path: &Path) -> bool {
    is_within_root(root, path) && !contains_reparse_component(path)
}

fn is_within_root(root: &Path, path: &Path) -> bool {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let candidate = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = path_identity_key(&root);
    let candidate = path_identity_key(&candidate);
    candidate == root
        || (candidate.starts_with(&root) && candidate.as_bytes().get(root.len()) == Some(&b'/'))
}

fn path_identity_key(path: &Path) -> String {
    let mut key = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    key.make_ascii_lowercase();
    while key.len() > 1 && key.ends_with('/') {
        key.pop();
    }
    key
}

fn contains_reparse_component(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return true;
            }
        }
    }
    false
}

/// A file is "stable" when its size stops changing across probes and it can
/// be opened for read. Half-synced OneDrive files fail one of the two.
///
/// A file that settles at zero bytes is stable, not pending. Requiring
/// `size > 0` meant a failed copy or an interrupted sync never stabilised at
/// all: the probe loop ran out, logged one line nobody reads, and returned
/// without ever enqueueing the file — so it produced no job row, no manifest,
/// no quarantine and no Needs Review card, while
/// `docs/TROUBLESHOOTING.md` documented `CORRUPT:zero-byte file` as the
/// outcome the user should expect. Let it through and let routing classify it,
/// which is the layer that owns that verdict.
async fn wait_stable(path: &Path) -> bool {
    let mut last: Option<u64> = None;
    for _ in 0..STABILITY_PROBES * 10 {
        let size = match std::fs::metadata(path) {
            Ok(m) => m.len(),
            Err(_) => return false, // vanished (moved by us or by sync)
        };
        if last == Some(size) && std::fs::File::open(path).is_ok() {
            return true;
        }
        last = Some(size);
        tokio::time::sleep(Duration::from_millis(STABILITY_INTERVAL_MS)).await;
    }
    // A file that reaches here gets no ledger row, no manifest and no quarantine
    // copy — it simply is not in the batch. The path is scrubbed to
    // `[path under C: (+n levels)]` by design, which left the operator of a
    // thousand-file overnight run with fewer manifests than files and nothing to
    // reconcile against. The extension and the last size observed are not the
    // sensitive part, and they are usually enough to identify which scan it was.
    log::warn!(
        "file never stabilized after {} tries, so it is not in this batch \
         (extension {:?}, last size {} bytes): {path:?}",
        STABILITY_PROBES * 10,
        path.extension().unwrap_or_default(),
        last.unwrap_or(0)
    );
    false
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if !is_safe_path_under_root(dir, &p) || is_reparse_point(&p) {
                    continue;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed copy or an interrupted sync leaves a zero-byte file. It has to
    /// reach the pipeline so routing can classify it and the user gets the
    /// `CORRUPT:zero-byte file` card `docs/TROUBLESHOOTING.md` promises them.
    /// The `size > 0` this replaces made such a file never stabilise, so it
    /// was dropped silently: no job row, no manifest, no quarantine, no card.
    #[tokio::test]
    async fn a_zero_byte_file_is_stable_rather_than_pending_forever() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed-copy.pdf");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

        // Bounded well under the probe loop's own budget: a regression here
        // shows up as a timeout, not as a passing test that waited it out.
        let stable = tokio::time::timeout(Duration::from_secs(10), wait_stable(&path))
            .await
            .expect("a settled zero-byte file must not exhaust the probe loop");
        assert!(stable, "a zero-byte file that is not growing is stable");
    }

    /// The other half of the same rule: still-growing files must not be
    /// enqueued mid-write, which is what the size comparison is actually for.
    #[tokio::test]
    async fn a_file_that_is_still_growing_is_not_stable_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("syncing.pdf");
        std::fs::write(&path, b"partial").unwrap();

        let writer = {
            let path = path.clone();
            tokio::spawn(async move {
                for n in 1..=6u8 {
                    tokio::time::sleep(Duration::from_millis(STABILITY_INTERVAL_MS / 2)).await;
                    let _ = std::fs::write(&path, vec![b'x'; 64 * n as usize]);
                }
            })
        };
        // While the size keeps changing the probe loop must keep waiting; it
        // only settles once the writer stops.
        let stable = wait_stable(&path).await;
        writer.await.unwrap();
        assert!(stable, "it settles once writing stops");
    }

    #[cfg(unix)]
    #[test]
    fn walk_does_not_follow_a_directory_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.pdf");
        std::fs::write(&outside_file, b"outside").unwrap();
        let link = dir.path().join("linked-folder");
        symlink(outside.path(), &link).unwrap();

        let files = walk(dir.path());
        assert!(!files.iter().any(|path| path == &outside_file));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_symlink_is_not_safe_even_when_its_target_is_inside_the_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.pdf");
        let link = dir.path().join("alias.pdf");
        std::fs::write(&target, b"inside").unwrap();
        symlink(&target, &link).unwrap();

        assert!(!is_safe_path_under_root(dir.path(), &link));
    }

    #[cfg(windows)]
    #[test]
    fn walk_does_not_follow_a_directory_junction() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.pdf");
        std::fs::write(&outside_file, b"outside").unwrap();
        let junction = dir.path().join("linked-folder");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .status()
            .unwrap();
        assert!(status.success(), "could not create junction for test");

        let files = walk(dir.path());
        assert!(!files.iter().any(|path| path == &outside_file));
    }
}
