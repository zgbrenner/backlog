//! Client for the `convertd` Python sidecar: one warm process, newline-
//! delimited JSON over stdin/stdout, no terminal window. Ops:
//!   pdf_probe | convert | ocr | langid | classify | salience | ettin_spans | ping
//! Respawn-on-death is handled here; the pipeline replays the job from the
//! ledger on RUNTIME_FAIL.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

/// Longest single stdout line convertd is allowed to send. The protocol is
/// one JSON object per line and the largest legitimate one is a converted
/// document's markdown, which `convert` already truncates to head/tail pages;
/// anything beyond this is a runaway, and `BufRead::read_line` would grow the
/// buffer to hold it until this process died.
const MAX_STDOUT_LINE_BYTES: usize = 32 * 1024 * 1024;

/// Stderr is prose, not payload — a traceback line far past this is noise.
const MAX_STDERR_LINE_BYTES: usize = 16 * 1024;

/// Stderr lines logged per spawned child before the rest are suppressed.
///
/// llama-server narrates every request, and a several-thousand-file backfill
/// would push the startup diagnostics — the whole reason stderr is captured —
/// out of the rotating log within minutes. Startup output arrives first, so a
/// per-process cap keeps exactly the part that explains a failure.
const MAX_LOGGED_STDERR_LINES: usize = 2000;

/// Outcome of one [`read_line_bounded`] call.
pub enum LineRead {
    Line(String),
    /// A line that ran past the ceiling and was discarded, with the number of
    /// bytes dropped.
    Overlong(u64),
    Eof,
    Err(std::io::Error),
}

/// `BufRead::read_line` with a hard ceiling.
///
/// A child that writes megabytes without ever emitting a newline (a hung
/// generator, a binary blob on the wrong stream) would otherwise be able to
/// grow this process's memory without limit. Over-long lines are drained and
/// dropped rather than buffered, so the reader stays in sync with the stream
/// and the next well-formed line is still delivered.
pub fn read_line_bounded(reader: &mut impl BufRead, max_bytes: usize) -> LineRead {
    let mut buf: Vec<u8> = Vec::new();
    let mut discarded: u64 = 0;
    loop {
        let (complete, consume) = {
            let available = match reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return LineRead::Err(e),
            };
            if available.is_empty() {
                // EOF. A trailing fragment with no newline is still worth
                // returning; a fragment we already gave up on is not.
                return match (discarded, buf.is_empty()) {
                    (0, true) => LineRead::Eof,
                    (0, false) => LineRead::Line(String::from_utf8_lossy(&buf).into_owned()),
                    _ => LineRead::Overlong(discarded),
                };
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    if discarded > 0 {
                        discarded += index as u64;
                    } else {
                        buf.extend_from_slice(&available[..index]);
                    }
                    (true, index + 1)
                }
                None => {
                    let len = available.len();
                    if discarded > 0 {
                        discarded += len as u64;
                    } else if buf.len() + len > max_bytes {
                        discarded = (buf.len() + len) as u64;
                        buf = Vec::new();
                    } else {
                        buf.extend_from_slice(available);
                    }
                    (false, len)
                }
            }
        };
        reader.consume(consume);
        if complete {
            return if discarded > 0 {
                LineRead::Overlong(discarded)
            } else {
                LineRead::Line(String::from_utf8_lossy(&buf).into_owned())
            };
        }
    }
}

/// Drain a child process's stderr into the app log on a dedicated thread.
///
/// Both of BackLog's children used to get `Stdio::null()`, which threw away
/// the one signal that explains a wedged run: convertd's tracebacks, and
/// llama-server's startup diagnostics (a port conflict otherwise surfaces
/// only as a 60-second timeout followed by SLM_FAIL flags with no cause
/// recorded anywhere). The shipped build is windowed and has no console, so
/// `logging.rs`'s rotating file is where this has to land.
pub fn log_child_stderr(
    label: &'static str,
    stderr: ChildStderr,
    max_lines: usize,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("{label}-stderr"))
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut logged = 0usize;
            loop {
                match read_line_bounded(&mut reader, MAX_STDERR_LINE_BYTES) {
                    LineRead::Line(line) => {
                        let line = line.trim_end();
                        if line.is_empty() {
                            continue;
                        }
                        logged += 1;
                        match logged.cmp(&max_lines) {
                            std::cmp::Ordering::Less => log::info!("{label}: {line}"),
                            std::cmp::Ordering::Equal => log::info!(
                                "{label}: {line} (further output from this process suppressed)"
                            ),
                            std::cmp::Ordering::Greater => {}
                        }
                    }
                    LineRead::Overlong(bytes) => {
                        log::warn!("{label}: discarded a {bytes}-byte stderr line with no newline")
                    }
                    LineRead::Eof => break,
                    LineRead::Err(e) => {
                        log::warn!("{label}: stderr could not be read: {e}");
                        break;
                    }
                }
            }
        })
}

#[derive(Debug, Serialize)]
struct Request<'a> {
    id: u64,
    op: &'a str,
    #[serde(flatten)]
    args: Value,
}

#[derive(Debug, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: Value,
}

pub struct Sidecar {
    exe: std::path::PathBuf,
    /// Injected into the spawned process's `BACKLOG_MODELS_DIR` (see
    /// `with_models_dir`). `None` leaves convertd.py's own default in effect
    /// (dev layout: `../models` beside the sidecar executable) — every
    /// production call site sets this via `model_download::resolve_models_dir`
    /// so an installed app's `models.lock.json` and any optional local model
    /// snapshots (the slim sidecar profile ships none by default -- see
    /// `sidecar/convertd.py`'s `_gliclass`/`_granite` loaders), which live
    /// under app-data rather than beside the exe, are actually found.
    models_dir: Option<std::path::PathBuf>,
    inner: Mutex<Option<Proc>>,
    counter: std::sync::atomic::AtomicU64,
    pub timeout: Duration,
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<std::io::Result<String>>,
    _reader: JoinHandle<()>,
    _stderr: JoinHandle<()>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        // Never orphan a sidecar process — on replace, timeout, or app exit.
        // Killing the child closes its stdout, which ends the reader thread.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Sidecar {
    /// Default-timeout constructor. Not currently called internally (the app
    /// always threads `Config::sidecar_timeout_secs` through `with_timeout`
    /// below) but kept as stable, obvious-default API surface for tests and
    /// other callers.
    #[allow(dead_code)]
    pub fn new(exe: std::path::PathBuf) -> Self {
        Self::with_timeout(exe, Duration::from_secs(120))
    }

    pub fn with_timeout(exe: std::path::PathBuf, timeout: Duration) -> Self {
        Self {
            exe,
            models_dir: None,
            inner: Mutex::new(None),
            counter: std::sync::atomic::AtomicU64::new(1),
            timeout,
        }
    }

    /// Sets the models directory injected into every spawned process as
    /// `BACKLOG_MODELS_DIR`. Builder-style so existing call sites (and the
    /// unix-only tests below, which never load a real model) are unaffected
    /// unless they opt in.
    pub fn with_models_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.models_dir = Some(dir);
        self
    }

    fn spawn(&self) -> anyhow::Result<Proc> {
        let mut cmd = Command::new(&self.exe);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped, not null: see `log_child_stderr`.
            .stderr(Stdio::piped());
        if let Some(dir) = &self.models_dir {
            cmd.env("BACKLOG_MODELS_DIR", dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no sidecar stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no sidecar stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("no sidecar stderr"))?;
        let stderr_thread = log_child_stderr("convertd", stderr, MAX_LOGGED_STDERR_LINES)?;

        // Drain stdout on a dedicated thread so `call` can enforce a per-request
        // deadline with recv_timeout. A blocking read on the pipe is otherwise
        // uncancellable, letting one pathological document wedge the sidecar
        // (and every job queued behind it) indefinitely.
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("convertd-reader".into())
            .spawn(move || {
                let mut r = BufReader::new(stdout);
                loop {
                    match read_line_bounded(&mut r, MAX_STDOUT_LINE_BYTES) {
                        LineRead::Line(line) => {
                            if tx.send(Ok(line)).is_err() {
                                break; // receiver gone; nothing to do
                            }
                        }
                        // Dropped rather than forwarded: `call` would fail to
                        // parse it anyway, and the point is that it never got
                        // buffered in the first place.
                        LineRead::Overlong(bytes) => {
                            log::warn!(
                                "convertd: discarded a {bytes}-byte response with no newline"
                            )
                        }
                        LineRead::Eof => break, // the child closed stdout
                        LineRead::Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
            })?;
        Ok(Proc {
            child,
            stdin,
            rx,
            _reader: reader,
            _stderr: stderr_thread,
        })
    }

    pub fn call(&self, op: &str, args: Value) -> anyhow::Result<Value> {
        let id = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut guard = self.inner.lock().unwrap();

        // Lazy spawn / respawn if dead.
        let need_spawn = match guard.as_mut() {
            None => true,
            Some(p) => matches!(p.child.try_wait(), Ok(Some(_)) | Err(_)),
        };
        if need_spawn {
            *guard = Some(self.spawn()?);
        }
        let proc = guard.as_mut().unwrap();

        let req = Request { id, op, args };
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        proc.stdin.write_all(line.as_bytes())?;
        proc.stdin.flush()?;

        enum Wake {
            Resp(Response),
            Timeout,
            ReadErr(String),
            Closed,
        }

        let deadline = std::time::Instant::now() + self.timeout;
        let wake = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break Wake::Timeout;
            }
            match proc.rx.recv_timeout(remaining) {
                Ok(Ok(buf)) => match serde_json::from_str::<Response>(buf.trim()) {
                    Ok(resp) if resp.id == id => break Wake::Resp(resp),
                    // Stray stderr-ish noise on stdout, or a stale response from
                    // a replayed job — keep waiting for our id.
                    _ => continue,
                },
                Ok(Err(e)) => break Wake::ReadErr(e.to_string()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Wake::Closed,
            }
        };

        match wake {
            Wake::Resp(resp) => {
                if !resp.ok {
                    anyhow::bail!("sidecar '{op}' failed: {}", resp.error.unwrap_or_default());
                }
                Ok(resp.data)
            }
            // Drop the wedged/broken process (its Drop kills it); the next call
            // respawns a clean one.
            Wake::Timeout => {
                *guard = None;
                anyhow::bail!("sidecar '{op}' timed out after {:?}", self.timeout);
            }
            Wake::ReadErr(e) => {
                *guard = None;
                anyhow::bail!("sidecar read error during '{op}': {e}");
            }
            Wake::Closed => {
                *guard = None;
                anyhow::bail!("sidecar closed stream during '{op}'");
            }
        }
    }

    // ---- typed helpers -----------------------------------------------------

    /// Returns (median_chars_per_page, page_count).
    pub fn pdf_probe(&self, path: &str) -> anyhow::Result<(u64, u64)> {
        let v = self.call("pdf_probe", serde_json::json!({ "path": path }))?;
        Ok((
            v["median_chars_per_page"].as_u64().unwrap_or(0),
            v["pages"].as_u64().unwrap_or(0),
        ))
    }

    pub fn convert(
        &self,
        path: &str,
        head_pages: usize,
        tail_pages: usize,
    ) -> anyhow::Result<ConvertResult> {
        let v = self.call(
            "convert",
            serde_json::json!({ "path": path, "head_pages": head_pages, "tail_pages": tail_pages }),
        )?;
        Ok(serde_json::from_value(v)?)
    }

    pub fn ocr(
        &self,
        path: &str,
        dpi: u32,
        head_pages: usize,
        tail_pages: usize,
    ) -> anyhow::Result<ConvertResult> {
        let v = self.call(
            "ocr",
            serde_json::json!({ "path": path, "dpi": dpi, "head_pages": head_pages, "tail_pages": tail_pages }),
        )?;
        Ok(serde_json::from_value(v)?)
    }

    pub fn langid(&self, text: &str) -> anyhow::Result<String> {
        let v = self.call("langid", serde_json::json!({ "text": text }))?;
        Ok(v["lang"].as_str().unwrap_or("en").to_string())
    }

    pub fn classify(&self, text: &str, labels: &[String]) -> anyhow::Result<(String, f64)> {
        let v = self.call(
            "classify",
            serde_json::json!({ "text": text, "labels": labels }),
        )?;
        Ok((
            v["label"].as_str().unwrap_or("correspondence").to_string(),
            v["score"].as_f64().unwrap_or(0.0),
        ))
    }

    pub fn salience(
        &self,
        sentences: &[String],
        probes: &[String],
        top_k: usize,
    ) -> anyhow::Result<Vec<usize>> {
        let v = self.call(
            "salience",
            serde_json::json!({ "sentences": sentences, "probes": probes, "top_k": top_k }),
        )?;
        Ok(v["indices"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_u64().map(|u| u as usize))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn ettin_spans(&self, text: &str) -> anyhow::Result<Vec<EttinSpan>> {
        let v = self.call("ettin_spans", serde_json::json!({ "text": text }))?;
        Ok(serde_json::from_value(v["spans"].clone()).unwrap_or_default())
    }

    pub fn versions(&self) -> anyhow::Result<Value> {
        self.call("versions", serde_json::json!({}))
    }

    /// Cheap liveness probe used by `preflight::run` (via `spawn_blocking`)
    /// to verify the sidecar executable launches and answers before a batch
    /// starts.
    pub fn ping(&self) -> anyhow::Result<()> {
        self.call("ping", serde_json::json!({})).map(|_| ())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConvertResult {
    pub markdown: String,
    #[serde(default)]
    pub doc_meta_dates: Vec<String>, // ISO dates from doc properties
    #[serde(default)]
    pub ocr_used: bool,
    #[serde(default)]
    pub ocr_mean_conf: f64,
    #[serde(default)]
    pub encrypted: bool,
    #[serde(default)]
    pub letterhead_resets: u32, // multi-doc packet heuristic count
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EttinSpan {
    pub label: String, // DATE | PARTY | SUBJECT
    pub text: String,
    pub score: f64,
    #[serde(default)]
    pub iso: Option<String>, // normalized, DATE spans only
}

#[cfg(test)]
mod line_tests {
    use super::*;

    fn read_all(input: &[u8], max_bytes: usize) -> Vec<String> {
        let mut reader = BufReader::with_capacity(8, input);
        let mut out = Vec::new();
        loop {
            match read_line_bounded(&mut reader, max_bytes) {
                LineRead::Line(line) => out.push(line),
                LineRead::Overlong(bytes) => out.push(format!("<dropped {bytes}>")),
                LineRead::Eof => return out,
                LineRead::Err(e) => panic!("{e}"),
            }
        }
    }

    #[test]
    fn splits_on_newlines_across_buffer_refills() {
        // An 8-byte BufReader capacity forces the multi-`fill_buf` path that
        // a real pipe hits on any response worth reading.
        assert_eq!(
            read_all(b"{\"id\":1}\n{\"id\":2,\"ok\":true}\n", 1024),
            vec![
                "{\"id\":1}".to_string(),
                "{\"id\":2,\"ok\":true}".to_string()
            ]
        );
    }

    #[test]
    fn returns_a_trailing_fragment_that_never_got_its_newline() {
        assert_eq!(
            read_all(b"a\nb", 1024),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    /// The point of the ceiling: a child that never emits a newline must not
    /// be able to grow this process's memory, and the reader must stay in
    /// sync so the *next* line still arrives.
    #[test]
    fn discards_an_overlong_line_and_keeps_reading() {
        let mut input = vec![b'x'; 4096];
        input.push(b'\n');
        input.extend_from_slice(b"{\"id\":9}\n");

        let lines = read_all(&input, 64);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("<dropped "), "{}", lines[0]);
        assert_eq!(lines[1], "{\"id\":9}");
    }

    #[test]
    fn an_unterminated_overlong_line_ends_the_stream_without_buffering_it() {
        let input = vec![b'x'; 100_000];
        assert_eq!(read_all(&input, 128), vec!["<dropped 100000>".to_string()]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_executable(path: &std::path::Path, source: &str) {
        std::fs::write(path, source).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn timeout_kills_process_and_next_request_respawns() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("fake-sidecar.sh");
        write_executable(
            &executable,
            "#!/bin/sh\nwhile IFS= read -r line; do sleep 5; done\n",
        );

        let sidecar = Sidecar::with_timeout(executable.clone(), Duration::from_millis(75));
        let error = sidecar
            .call("ping", serde_json::json!({}))
            .expect_err("silent sidecar must time out");
        assert!(error.to_string().contains("timed out"));

        write_executable(
            &executable,
            "#!/bin/sh\nwhile IFS= read -r line; do printf '%s\\n' '{\"id\":2,\"ok\":true}'; done\n",
        );
        sidecar
            .ping()
            .expect("next request should spawn a clean process");
    }
}
