//! Client for the `convertd` Python sidecar: one warm process, newline-
//! delimited JSON over stdin/stdout, no terminal window. Ops:
//!   pdf_probe | convert | ocr | langid | classify | salience | ettin_spans | ping
//!
//! Stdout is drained by a dedicated reader thread. Every request is bounded by
//! `timeout`; a timeout or broken stream kills and clears the process so the
//! next request can lazily spawn a clean sidecar.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("failed to spawn sidecar '{path}': {source}")]
    Spawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sidecar process did not expose stdin")]
    MissingStdin,
    #[error("sidecar process did not expose stdout")]
    MissingStdout,
    #[error("failed to start sidecar stdout reader: {0}")]
    ReaderSpawn(#[source] std::io::Error),
    #[error("sidecar '{op}' write failed: {source}")]
    Write {
        op: String,
        #[source]
        source: std::io::Error,
    },
    #[error("sidecar '{op}' timed out after {timeout:?}")]
    Timeout { op: String, timeout: Duration },
    #[error("sidecar closed its stream during '{op}': {detail}")]
    Closed { op: String, detail: String },
    #[error("sidecar '{op}' protocol error: {detail}")]
    Protocol { op: String, detail: String },
    #[error("sidecar '{op}' failed: {detail}")]
    Remote { op: String, detail: String },
    #[error("sidecar process lock was poisoned")]
    LockPoisoned,
}

pub struct Sidecar {
    exe: PathBuf,
    inner: Mutex<Option<Proc>>,
    counter: AtomicU64,
    pub timeout: Duration,
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<ReaderEvent>,
}

enum ReaderEvent {
    Line(String),
    Closed,
    Error(String),
}

impl Sidecar {
    pub fn new(exe: PathBuf) -> Self {
        Self::with_timeout(exe, Duration::from_secs(120))
    }

    pub fn with_timeout(exe: PathBuf, timeout: Duration) -> Self {
        Self {
            exe,
            inner: Mutex::new(None),
            counter: AtomicU64::new(1),
            timeout,
        }
    }

    fn spawn(&self) -> Result<Proc, SidecarError> {
        let mut cmd = Command::new(&self.exe);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn().map_err(|source| SidecarError::Spawn {
            path: self.exe.clone(),
            source,
        })?;
        let stdin = child.stdin.take().ok_or(SidecarError::MissingStdin)?;
        let stdout = child.stdout.take().ok_or(SidecarError::MissingStdout)?;
        let (tx, responses) = mpsc::channel();

        if let Err(source) = std::thread::Builder::new()
            .name("backlog-sidecar-stdout".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => {
                            let _ = tx.send(ReaderEvent::Closed);
                            break;
                        }
                        Ok(_) => {
                            let complete = line.trim_end_matches(['\r', '\n']).to_string();
                            if tx.send(ReaderEvent::Line(complete)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = tx.send(ReaderEvent::Error(error.to_string()));
                            break;
                        }
                    }
                }
            })
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SidecarError::ReaderSpawn(source));
        }

        Ok(Proc {
            child,
            stdin,
            responses,
        })
    }

    fn stop_proc(slot: &mut Option<Proc>) {
        if let Some(mut process) = slot.take() {
            let _ = process.child.kill();
            let _ = process.child.wait();
        }
    }

    fn ensure_process(&self, slot: &mut Option<Proc>) -> Result<(), SidecarError> {
        let needs_spawn = match slot.as_mut() {
            None => true,
            Some(process) => matches!(process.child.try_wait(), Ok(Some(_)) | Err(_)),
        };
        if needs_spawn {
            Self::stop_proc(slot);
            *slot = Some(self.spawn()?);
        }
        Ok(())
    }

    pub fn terminate(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            Self::stop_proc(&mut guard);
        }
    }

    pub fn ping(&self) -> Result<(), SidecarError> {
        self.call("ping", serde_json::json!({})).map(|_| ())
    }

    pub fn call(&self, op: &str, args: Value) -> Result<Value, SidecarError> {
        let id = self.counter.fetch_add(1, Ordering::SeqCst);
        let mut guard = self.inner.lock().map_err(|_| SidecarError::LockPoisoned)?;
        self.ensure_process(&mut guard)?;

        let request = Request { id, op, args };
        let mut line = serde_json::to_string(&request).map_err(|error| {
            SidecarError::Protocol {
                op: op.to_string(),
                detail: error.to_string(),
            }
        })?;
        line.push('\n');

        let write_result = guard
            .as_mut()
            .expect("sidecar process exists after ensure_process")
            .stdin
            .write_all(line.as_bytes())
            .and_then(|_| {
                guard
                    .as_mut()
                    .expect("sidecar process exists after ensure_process")
                    .stdin
                    .flush()
            });
        if let Err(source) = write_result {
            Self::stop_proc(&mut guard);
            return Err(SidecarError::Write {
                op: op.to_string(),
                source,
            });
        }

        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                Self::stop_proc(&mut guard);
                return Err(SidecarError::Timeout {
                    op: op.to_string(),
                    timeout: self.timeout,
                });
            }

            let event = guard
                .as_ref()
                .expect("sidecar process exists while awaiting response")
                .responses
                .recv_timeout(remaining);
            match event {
                Ok(ReaderEvent::Line(raw)) => {
                    let response: Response = match serde_json::from_str(raw.trim()) {
                        Ok(response) => response,
                        Err(_) => continue,
                    };
                    if response.id != id {
                        continue;
                    }
                    if !response.ok {
                        return Err(SidecarError::Remote {
                            op: op.to_string(),
                            detail: response.error.unwrap_or_default(),
                        });
                    }
                    return Ok(response.data);
                }
                Ok(ReaderEvent::Closed) => {
                    Self::stop_proc(&mut guard);
                    return Err(SidecarError::Closed {
                        op: op.to_string(),
                        detail: "end of stream".into(),
                    });
                }
                Ok(ReaderEvent::Error(detail)) => {
                    Self::stop_proc(&mut guard);
                    return Err(SidecarError::Closed {
                        op: op.to_string(),
                        detail,
                    });
                }
                Err(RecvTimeoutError::Timeout) => {
                    Self::stop_proc(&mut guard);
                    return Err(SidecarError::Timeout {
                        op: op.to_string(),
                        timeout: self.timeout,
                    });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    Self::stop_proc(&mut guard);
                    return Err(SidecarError::Closed {
                        op: op.to_string(),
                        detail: "reader channel disconnected".into(),
                    });
                }
            }
        }
    }

    // ---- typed helpers -----------------------------------------------------

    /// Returns (median_chars_per_page, page_count).
    pub fn pdf_probe(&self, path: &str) -> Result<(u64, u64), SidecarError> {
        let value = self.call("pdf_probe", serde_json::json!({ "path": path }))?;
        Ok((
            value["median_chars_per_page"].as_u64().unwrap_or(0),
            value["pages"].as_u64().unwrap_or(0),
        ))
    }

    pub fn convert(
        &self,
        path: &str,
        head_pages: usize,
        tail_pages: usize,
    ) -> Result<ConvertResult, SidecarError> {
        let value = self.call(
            "convert",
            serde_json::json!({
                "path": path,
                "head_pages": head_pages,
                "tail_pages": tail_pages
            }),
        )?;
        serde_json::from_value(value).map_err(|error| SidecarError::Protocol {
            op: "convert".into(),
            detail: error.to_string(),
        })
    }

    pub fn ocr(
        &self,
        path: &str,
        dpi: u32,
        head_pages: usize,
        tail_pages: usize,
    ) -> Result<ConvertResult, SidecarError> {
        let value = self.call(
            "ocr",
            serde_json::json!({
                "path": path,
                "dpi": dpi,
                "head_pages": head_pages,
                "tail_pages": tail_pages
            }),
        )?;
        serde_json::from_value(value).map_err(|error| SidecarError::Protocol {
            op: "ocr".into(),
            detail: error.to_string(),
        })
    }

    pub fn langid(&self, text: &str) -> Result<String, SidecarError> {
        let value = self.call("langid", serde_json::json!({ "text": text }))?;
        Ok(value["lang"].as_str().unwrap_or("en").to_string())
    }

    pub fn classify(
        &self,
        text: &str,
        labels: &[String],
    ) -> Result<(String, f64), SidecarError> {
        let value = self.call(
            "classify",
            serde_json::json!({ "text": text, "labels": labels }),
        )?;
        Ok((
            value["label"]
                .as_str()
                .unwrap_or("correspondence")
                .to_string(),
            value["score"].as_f64().unwrap_or(0.0),
        ))
    }

    pub fn salience(
        &self,
        sentences: &[String],
        probes: &[String],
        top_k: usize,
    ) -> Result<Vec<usize>, SidecarError> {
        let value = self.call(
            "salience",
            serde_json::json!({
                "sentences": sentences,
                "probes": probes,
                "top_k": top_k
            }),
        )?;
        Ok(value["indices"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_u64().map(|index| index as usize))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn ettin_spans(&self, text: &str) -> Result<Vec<EttinSpan>, SidecarError> {
        let value = self.call("ettin_spans", serde_json::json!({ "text": text }))?;
        serde_json::from_value(value["spans"].clone()).map_err(|error| {
            SidecarError::Protocol {
                op: "ettin_spans".into(),
                detail: error.to_string(),
            }
        })
    }

    pub fn versions(&self) -> Result<Value, SidecarError> {
        self.call("versions", serde_json::json!({}))
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        if let Ok(slot) = self.inner.get_mut() {
            Self::stop_proc(slot);
        }
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
    pub page_count: u64,
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
        let error = sidecar
            .call("ping", serde_json::json!({}))
            .expect_err("silent sidecar must time out");
        assert!(matches!(error, SidecarError::Timeout { .. }));

        write_executable(
            &executable,
            "#!/bin/sh\nwhile IFS= read -r line; do printf '%s\\n' '{\"id\":2,\"ok\":true}'; done\n",
        );
        sidecar.ping().expect("next request should spawn a clean process");
    }
}
