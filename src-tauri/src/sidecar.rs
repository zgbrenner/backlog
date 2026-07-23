//! Client for the `convertd` Python sidecar: one warm process, newline-
//! delimited JSON over stdin/stdout, no terminal window. Ops:
//!   pdf_probe | convert | ocr | langid | classify | salience | ettin_spans | ping
//! Respawn-on-death is handled here; the pipeline replays the job from the
//! ledger on RUNTIME_FAIL.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

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
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
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
        let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no sidecar stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no sidecar stdout"))?;

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
                    let mut line = String::new();
                    match r.read_line(&mut line) {
                        Ok(0) => break, // EOF: the child closed stdout
                        Ok(_) => {
                            if tx.send(Ok(line)).is_err() {
                                break; // receiver gone; nothing to do
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
            })?;
        Ok(Proc { child, stdin, rx, _reader: reader })
    }

    pub fn call(&self, op: &str, args: Value) -> anyhow::Result<Value> {
        let id = self.counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    pub fn ocr(&self, path: &str, dpi: u32, head_pages: usize, tail_pages: usize) -> anyhow::Result<ConvertResult> {
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
        let v = self.call("classify", serde_json::json!({ "text": text, "labels": labels }))?;
        Ok((
            v["label"].as_str().unwrap_or("correspondence").to_string(),
            v["score"].as_f64().unwrap_or(0.0),
        ))
    }

    pub fn salience(&self, sentences: &[String], probes: &[String], top_k: usize) -> anyhow::Result<Vec<usize>> {
        let v = self.call(
            "salience",
            serde_json::json!({ "sentences": sentences, "probes": probes, "top_k": top_k }),
        )?;
        Ok(v["indices"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_u64().map(|u| u as usize)).collect())
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
        let error = sidecar.call("ping", serde_json::json!({})).expect_err("silent sidecar must time out");
        assert!(error.to_string().contains("timed out"));

        write_executable(
            &executable,
            "#!/bin/sh\nwhile IFS= read -r line; do printf '%s\\n' '{\"id\":2,\"ok\":true}'; done\n",
        );
        sidecar.ping().expect("next request should spawn a clean process");
    }
}
