//! On-disk diagnostics for a build that has no console.
//!
//! `main.rs` sets `windows_subsystem = "windows"`, so nothing is attached to
//! stderr and `env_logger`'s default filter (error only, absent `RUST_LOG`)
//! discarded roughly twenty warn/error sites into a void. The target user
//! cannot set an environment variable, `strip = true` means a Windows Error
//! Reporting dump has no symbols, and "files stopped processing" is the whole
//! bug report. So: everything at info and above goes to a size-rotated file
//! under the app data dir, panics are written there too, and
//! `crate::get_diagnostics` hands the tail of it back to the webview for a
//! copy/paste support bundle.
//!
//! Deliberately no new dependency: `env_logger` accepts an arbitrary
//! `Target::Pipe` sink, which is all a rotating file writer needs to be.
//!
//! Persisting that file inverted an assumption the pipeline was written
//! against: several sites log the detail *because* the log used to be the
//! non-persisted channel (pipeline.rs quarantine/convert errors embed the
//! document's absolute path, watcher.rs narrates the Processing root and every
//! candidate file). On a product that SQLCipher-encrypts its ledger precisely
//! so HR filenames do not sit in the clear, a plaintext
//! `%APPDATA%\<id>\logs\backlog.log` full of them is not an acceptable trade
//! for supportability. So everything on its way to the file — and everything
//! on its way back out through `get_diagnostics` — goes through `scrub`.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// Rotate at 4 MB and keep one previous generation: enough to hold a whole
/// multi-thousand-file backfill's warnings, small enough to paste into an
/// email if it comes to that.
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

/// Folders whose *names* are the sensitive part: the OneDrive Processing root
/// ("2024 Terminations"), Quarantine, Outbox, the cache, and the app-data dir
/// (which carries the Windows account name). Registered by `crate::run` at
/// startup and again whenever settings change, so the scrubber knows what a
/// document path looks like on *this* machine without guessing.
static SENSITIVE_ROOTS: OnceLock<RwLock<Vec<(String, String)>>> = OnceLock::new();

/// slm.rs formats the model's verbatim proposal into its parse-failure error
/// (`...; raw: {content}`), and pipeline.rs logs that error whole. That text is
/// a subject and description derived from the document body — the single most
/// sensitive thing the app holds — so it is cut here rather than persisted.
const MODEL_OUTPUT_MARKER: &str = "; raw: ";

fn sensitive_roots() -> &'static RwLock<Vec<(String, String)>> {
    SENSITIVE_ROOTS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Case- and separator-insensitive form used for matching. Both operations are
/// byte-length preserving on any input, so an offset found in the normalized
/// haystack indexes the original string unchanged.
fn normalized(text: &str) -> String {
    text.replace('\\', "/").to_ascii_lowercase()
}

/// Teach the scrubber about a folder whose contents must never reach the log
/// file. Additive and idempotent: a config change adds the new folders without
/// forgetting the old ones, because lines already queued still name them.
pub fn add_sensitive_roots<I: IntoIterator<Item = PathBuf>>(paths: I) {
    let mut roots = sensitive_roots().write().unwrap_or_else(|e| e.into_inner());
    for path in paths {
        // A one- or two-character "root" (an empty or `/` path) would match
        // every line in the file; a folder that shallow is not a real config.
        let key = normalized(&path.to_string_lossy());
        if key.len() < 4 || roots.iter().any(|(existing, _)| existing == &key) {
            continue;
        }
        roots.push((key, redact_path(&path)));
    }
    // Longest first: a cache dir nested inside the Processing root must be
    // reported as itself rather than as its parent.
    roots.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
}

/// Remove document-derived text from one record's worth of log output.
///
/// Applied on the way into the file *and* on the way out through
/// `get_diagnostics`, so a log written before a folder was registered — or by
/// a previous version — is still safe to paste into a support email.
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for segment in text.split_inclusive('\n') {
        let body = segment.trim_end_matches('\n').trim_end_matches('\r');
        out.push_str(&scrub_paths(&elide_model_output(body)));
        out.push_str(&segment[body.len()..]);
    }
    out
}

fn elide_model_output(line: &str) -> String {
    match line.find(MODEL_OUTPUT_MARKER) {
        Some(at) => format!(
            "{}{MODEL_OUTPUT_MARKER}[model output withheld]",
            &line[..at]
        ),
        None => line.to_string(),
    }
}

fn scrub_paths(line: &str) -> String {
    let roots = sensitive_roots().read().unwrap_or_else(|e| e.into_inner());
    if roots.is_empty() {
        return line.to_string();
    }
    let haystack = normalized(line);
    let mut out = String::with_capacity(line.len());
    let mut cursor = 0;
    while cursor < line.len() {
        let found = roots
            .iter()
            .filter_map(|(root, redacted)| {
                haystack[cursor..]
                    .find(root.as_str())
                    .map(|at| (cursor + at, root.len(), redacted))
            })
            .min_by_key(|(at, _, _)| *at);
        let Some((at, root_len, redacted)) = found else {
            break;
        };
        out.push_str(&line[cursor..at]);
        out.push_str(&format!("[path under {redacted}]"));
        cursor = path_run_end(line, at + root_len);
    }
    out.push_str(&line[cursor..]);
    out
}

/// How far past a matched root the redaction extends.
///
/// Rust's `Debug` formatting of a path quotes it, which is what every watcher
/// and pipeline site uses (`{path:?}`), so stopping at the closing quote keeps
/// the rest of the sentence — "…: filename starts with a reserved prefix" — and
/// still removes the whole filename, spaces included. When the path arrives
/// unquoted inside another error's text there is no reliable terminator, and
/// the remainder of that line is far likelier to be more of the path than
/// anything worth keeping, so it all goes.
fn path_run_end(line: &str, from: usize) -> usize {
    line[from..]
        .find(['"', '\''])
        .map(|at| from + at)
        .unwrap_or(line.len())
}

/// A `Write` sink that renames `backlog.log` to `backlog.log.1` once it
/// crosses `max_bytes`. Locked because `env_logger` writes from every thread
/// in the app.
struct RotatingFile {
    path: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
    max_bytes: u64,
}

impl RotatingFile {
    fn open(path: PathBuf, max_bytes: u64) -> Self {
        let mut sink = Self {
            path,
            file: None,
            written: 0,
            max_bytes,
        };
        sink.reopen();
        sink
    }

    fn reopen(&mut self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        self.written = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        // A logger must never be the reason the app fails to start, so every
        // failure here degrades to "no file logging" rather than propagating.
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
    }

    fn rotate_if_needed(&mut self) {
        if self.written < self.max_bytes {
            return;
        }
        self.file = None; // close before renaming: Windows will not rename an open file
        let previous = self.path.with_extension("log.1");
        let _ = std::fs::remove_file(&previous);
        let _ = std::fs::rename(&self.path, &previous);
        self.reopen();
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.rotate_if_needed();
        match self.file.as_mut() {
            Some(file) => {
                let n = file.write(buf)?;
                self.written += n as u64;
                Ok(n)
            }
            // Pretend success: a log line that cannot be persisted is not
            // worth an error return that env_logger would panic on.
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

struct SharedSink(Arc<Mutex<RotatingFile>>);

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // env_logger formats a whole record and hands it over in one call, so
        // scrubbing here sees complete lines and cannot be defeated by a
        // sensitive path straddling two writes.
        let scrubbed = scrub(&String::from_utf8_lossy(buf));
        // A panic anywhere in the app poisons this lock, and the panic hook
        // then wants to log — recover through the poison rather than take the
        // process down for the sake of a log line.
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(scrubbed.as_bytes())?;
        // Report the caller's byte count, not the redacted one: a short write
        // would send `write_all` round again with an offset into a buffer that
        // no longer corresponds to what was written.
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

/// Start file logging under `<app_data>/logs/` and install a panic hook that
/// lands in the same file. Returns the log path so the UI can offer to open
/// the folder. Safe to call more than once; the second call is a no-op.
pub fn init(app_data_dir: &Path, version: &str) -> PathBuf {
    let path = app_data_dir.join("logs").join("backlog.log");
    // Registered before the first line is written: the startup banner below
    // names this path, and on Windows it contains the account name.
    add_sensitive_roots([app_data_dir.to_path_buf()]);
    let sink = Arc::new(Mutex::new(RotatingFile::open(path.clone(), MAX_LOG_BYTES)));

    let mut builder = env_logger::Builder::new();
    builder
        .filter_level(log::LevelFilter::Info)
        // RUST_LOG still wins for a developer; the shipped default no longer
        // depends on anyone being able to set it.
        .parse_env("RUST_LOG")
        .format_timestamp_secs()
        .target(env_logger::Target::Pipe(Box::new(SharedSink(sink.clone()))));
    if builder.try_init().is_err() {
        return path;
    }

    log::info!("BackLog {version} starting; log file {}", path.display());
    install_panic_hook(version.to_string());
    path
}

/// Route panics through the same file. Without this a panic in a worker
/// thread of a windowless build leaves literally no trace on the machine.
fn install_panic_hook(version: String) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_string());
        log::error!("PANIC in BackLog {version} at {location}: {payload}");
        log::logger().flush();
        previous(info);
    }));
}

/// Last `limit` lines of the log file, oldest first, scrubbed again.
///
/// Reads at most the trailing `MAX_TAIL_BYTES` so a rotation-sized file never
/// has to be loaded whole to answer a support request. The second scrub is not
/// redundant: this is the payload `get_diagnostics` invites the user to paste
/// into an email, and the file may predate the current set of configured
/// folders (or a version that scrubbed at all).
pub fn tail(path: &Path, limit: usize) -> Vec<String> {
    const MAX_TAIL_BYTES: u64 = 512 * 1024;

    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = scrub(&String::from_utf8_lossy(&buf));
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    // A mid-line seek makes the first line a fragment; drop it rather than
    // ship a truncated record that reads like a real one.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.len() > limit {
        lines.drain(..lines.len() - limit);
    }
    lines
}

/// Reduce a filesystem path to its root plus how deep it goes.
///
/// Diagnostics are meant to be pasted into an email or a ticket, and the
/// folder names on this appliance are HR-shaped ("2024 Terminations",
/// "\\\\fs01\\Legal\\Redundancies"). Depth and drive are what a support
/// question actually needs; the leaf names are not.
pub fn redact_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        return "(not set)".into();
    }
    let text = path.to_string_lossy();
    let unified = text.replace('\\', "/");
    let mut parts = unified.split('/').filter(|p| !p.is_empty());
    let root = match parts.next() {
        // "C:" from a Windows path, "" from a POSIX absolute path.
        Some(first) if first.ends_with(':') => first.to_string(),
        Some(_) if unified.starts_with('/') => "/".to_string(),
        Some(first) => format!("{first}/…"),
        None => "/".to_string(),
    };
    let depth = unified.split('/').filter(|p| !p.is_empty()).count();
    format!("{root} (+{depth} levels)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_once_past_the_size_cap_and_keeps_the_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("backlog.log");
        let mut sink = RotatingFile::open(path.clone(), 64);

        for i in 0..20 {
            writeln!(sink, "line {i} padded out to force a rotation").unwrap();
        }
        sink.flush().unwrap();

        assert!(path.is_file(), "a live log file must exist after rotation");
        assert!(
            path.with_extension("log.1").is_file(),
            "the previous generation must be kept"
        );
        assert!(
            std::fs::metadata(&path).unwrap().len() < 64 * 4,
            "the live file must not keep growing past the cap"
        );
    }

    #[test]
    fn tail_returns_the_last_lines_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backlog.log");
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail(&path, 2), vec!["c".to_string(), "d".to_string()]);
        assert_eq!(tail(&path, 99).len(), 4);
        assert!(tail(&dir.path().join("absent.log"), 10).is_empty());
    }

    /// The whole point of persisting the log: the file that lands in
    /// `%APPDATA%` must not become a plaintext index of HR document names on a
    /// product that encrypts its ledger for exactly that reason.
    #[test]
    fn a_configured_folder_and_the_filename_under_it_never_reach_the_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let processing = dir.path().join("OneDrive").join("2024 Terminations");
        add_sensitive_roots([processing.clone()]);

        let path = dir.path().join("logs").join("backlog.log");
        let sink = Arc::new(Mutex::new(RotatingFile::open(path.clone(), MAX_LOG_BYTES)));
        let mut shared = SharedSink(sink.clone());

        // The three real shapes: watcher.rs's Debug-quoted path, an unquoted
        // path inside a sidecar error, and slm.rs's quoted raw model output.
        let doc = processing.join("Jane Roe Termination Letter.pdf");
        writeln!(
            shared,
            "[INFO] ignoring {doc:?}: filename starts with a reserved prefix"
        )
        .unwrap();
        writeln!(
            shared,
            "[WARN] convert attempt 1 failed: sidecar error for {}: No such file",
            doc.display()
        )
        .unwrap();
        writeln!(
            shared,
            "[WARN] name attempt 2 SLM error: parse failed; raw: {{\"subject\":\"Jane Roe redundancy\"}}"
        )
        .unwrap();
        shared.flush().unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("Terminations"),
            "the Processing folder name leaked: {written}"
        );
        assert!(
            !written.contains("Jane Roe"),
            "a document filename or a model proposal leaked: {written}"
        );
        assert!(
            !written.contains("Termination Letter.pdf"),
            "a document filename leaked: {written}"
        );
        // Still a usable diagnostic: the sentence around the path survives.
        assert!(written.contains("filename starts with a reserved prefix"));
        assert!(written.contains("convert attempt 1 failed"));
        assert!(written.contains("name attempt 2 SLM error"));
        assert_eq!(written.lines().count(), 3, "one line per record: {written}");

        // And the tail handed to get_diagnostics carries neither either.
        let tailed = tail(&path, 10).join("\n");
        assert!(!tailed.contains("Terminations"), "got: {tailed}");
        assert!(!tailed.contains("Jane Roe"), "got: {tailed}");
    }

    /// A log written before the folders were registered — or by a version that
    /// did not scrub at all — is still the payload the user is invited to paste
    /// into an email, so the read path scrubs too.
    #[test]
    fn the_tail_scrubs_lines_that_were_already_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let quarantine = dir.path().join("Quarantine-Legal-Redundancies");
        let path = dir.path().join("backlog.log");
        std::fs::write(
            &path,
            format!(
                "[WARN] file never stabilized: {:?}\n",
                quarantine.join("Payroll Dispute.pdf")
            ),
        )
        .unwrap();
        add_sensitive_roots([quarantine]);

        let tailed = tail(&path, 10).join("\n");
        assert!(!tailed.contains("Redundancies"), "got: {tailed}");
        assert!(!tailed.contains("Payroll Dispute"), "got: {tailed}");
        assert!(tailed.contains("file never stabilized"), "got: {tailed}");
    }

    #[test]
    fn scrubbing_leaves_a_line_with_nothing_sensitive_in_it_alone() {
        assert_eq!(
            scrub("[INFO] pipeline started\n"),
            "[INFO] pipeline started\n"
        );
        // An unregistered root is not guessed at, but model output is cut
        // wherever it appears because slm.rs marks it.
        assert_eq!(
            scrub("[WARN] x; raw: {\"subject\":\"secret\"}"),
            "[WARN] x; raw: [model output withheld]"
        );
    }

    #[test]
    fn redact_path_keeps_the_drive_and_the_depth_but_not_the_folder_names() {
        let redacted = redact_path(Path::new("C:\\Users\\jane\\OneDrive\\2024 Terminations"));
        assert!(redacted.starts_with("C:"), "got: {redacted}");
        assert!(redacted.contains("+5 levels"), "got: {redacted}");
        assert!(!redacted.contains("Terminations"));
        assert!(!redacted.contains("jane"));

        let posix = redact_path(Path::new("/home/jane/Processing"));
        assert!(!posix.contains("jane"), "got: {posix}");
        assert_eq!(redact_path(Path::new("")), "(not set)");
    }
}
