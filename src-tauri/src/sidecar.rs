//! Client for the `convertd` Python sidecar: one warm process, newline-
//! delimited JSON over stdin/stdout, no terminal window. Ops:
//!   pdf_probe | convert | ocr | langid | classify | salience | ettin_spans | ping
//! Respawn-on-death is handled here; the pipeline replays the job from the
//! ledger on RUNTIME_FAIL.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
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
    inner: Mutex<Option<Proc>>,
    counter: std::sync::atomic::AtomicU64,
    pub timeout: Duration,
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Sidecar {
    pub fn new(exe: std::path::PathBuf) -> Self {
        Self {
            exe,
            inner: Mutex::new(None),
            counter: std::sync::atomic::AtomicU64::new(1),
            timeout: Duration::from_secs(120),
        }
    }

    fn spawn(&self) -> anyhow::Result<Proc> {
        let mut cmd = Command::new(&self.exe);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no sidecar stdin"))?;
        let stdout =
            BufReader::new(child.stdout.take().ok_or_else(|| anyhow::anyhow!("no sidecar stdout"))?);
        Ok(Proc { child, stdin, stdout })
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

        let mut buf = String::new();
        loop {
            buf.clear();
            let n = proc.stdout.read_line(&mut buf)?;
            if n == 0 {
                *guard = None; // process died mid-call
                anyhow::bail!("sidecar closed stream during '{op}'");
            }
            let resp: Response = match serde_json::from_str(buf.trim()) {
                Ok(r) => r,
                Err(_) => continue, // tolerate stray stderr-ish noise on stdout
            };
            if resp.id != id {
                continue; // stale response from a replayed job
            }
            if !resp.ok {
                anyhow::bail!("sidecar '{op}' failed: {}", resp.error.unwrap_or_default());
            }
            return Ok(resp.data);
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
