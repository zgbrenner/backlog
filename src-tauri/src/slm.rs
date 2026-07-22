//! §6 SLM lane. One llama-server process (continuous batching, --parallel N)
//! serving the primary LFM2.5-350M; the 1.2B escalation weights load lazily
//! into a second server on first flagged retry and stay resident for the
//! batch so a bad stretch of files doesn't thrash model loads.
//!
//! Decoding is locked with a GBNF grammar (resources/name.gbnf): the model
//! cannot emit anything but the fields JSON. The app, never the model,
//! composes the filename.

use crate::checker::SlmOutput;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

pub struct SlmLane {
    grammar: String,
    llama_server_exe: PathBuf,
    primary_gguf: PathBuf,
    escalation_gguf: PathBuf,
    parallel: u8,
    primary_port: u16,
    escalation_port: u16,
    primary: Mutex<Option<Child>>,
    escalation: Mutex<Option<Child>>,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Primary,
    Escalation,
}

impl SlmLane {
    pub fn new(
        llama_server_exe: PathBuf,
        grammar: String,
        primary_gguf: PathBuf,
        escalation_gguf: PathBuf,
        base_port: u16,
        parallel: u8,
    ) -> Self {
        Self {
            grammar,
            llama_server_exe,
            primary_gguf,
            escalation_gguf,
            parallel,
            primary_port: base_port,
            escalation_port: base_port + 1,
            primary: Mutex::new(None),
            escalation: Mutex::new(None),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                // Localhost only; the app makes zero outbound calls at runtime.
                .no_proxy()
                .build()
                .expect("http client"),
        }
    }

    fn spawn_server(&self, gguf: &Path, port: u16) -> anyhow::Result<Child> {
        let mut cmd = Command::new(&self.llama_server_exe);
        cmd.args([
            "--model",
            gguf.to_str().unwrap_or_default(),
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--parallel",
            &self.parallel.to_string(),
            "--ctx-size",
            &(4096u32 * self.parallel as u32).to_string(),
            "--no-webui",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        Ok(cmd.spawn()?)
    }

    async fn ensure_up(&self, tier: Tier) -> anyhow::Result<u16> {
        let (slot, gguf, port) = match tier {
            Tier::Primary => (&self.primary, &self.primary_gguf, self.primary_port),
            Tier::Escalation => (&self.escalation, &self.escalation_gguf, self.escalation_port),
        };
        {
            let mut guard = slot.lock().unwrap();
            let need = match guard.as_mut() {
                None => true,
                Some(c) => matches!(c.try_wait(), Ok(Some(_)) | Err(_)),
            };
            if need {
                *guard = Some(self.spawn_server(gguf, port)?);
            }
        }
        // Poll /health until ready (model load can take a bit on first hit).
        let url = format!("http://127.0.0.1:{port}/health");
        for _ in 0..120 {
            if let Ok(r) = self.http.get(&url).send().await {
                if r.status().is_success() {
                    return Ok(port);
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        anyhow::bail!("llama-server on port {port} never became healthy")
    }

    /// LFM2.5 uses a ChatML-like template.
    fn build_prompt(system: &str, user: &str) -> String {
        format!(
            "<|startoftext|><|im_start|>system\n{system}<|im_end|>\n<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n"
        )
    }

    pub async fn name_document(
        &self,
        tier: Tier,
        evidence: &str,
        doc_type: &str,
        language: &str,
        violation_note: Option<&str>,
    ) -> anyhow::Result<SlmOutput> {
        let port = self.ensure_up(tier).await?;
        let today = chrono::Utc::now().date_naive().format("%Y-%m-%d");

        let mut system = format!(
            "You name business and legal documents from evidence excerpts.\n\
             Today's date: {today}. Document language: {language}. Classified type: {doc_type}.\n\
             Respond with exactly one JSON object with keys date, date_source, subject, description.\n\
             Rules:\n\
             - date: the document's own date in YYYY-MM-DD, chosen ONLY from dates visible in the evidence. If no date is visible, use \"none\".\n\
             - date_source: \"document\" if the date appears in the document text, \"metadata\" if only in file metadata, \"none\" if no date.\n\
             - subject: 3 to 8 words, specific, names the document type and key party or matter. Never generic words like Document or Scan. No slashes, colons, or special characters.\n\
             - description: exactly one sentence, 15 to 200 characters, adding information beyond the subject.\n\
             Do not invent dates, parties, or facts not present in the evidence."
        );
        if let Some(v) = violation_note {
            system.push_str(&format!(
                "\nYour previous answer was rejected by a validator: {v}. Correct exactly that problem."
            ));
        }

        let prompt = Self::build_prompt(&system, evidence);
        let body = json!({
            "prompt": prompt,
            "grammar": self.grammar,
            "temperature": 0.0,
            "n_predict": 220,
            "stop": ["<|im_end|>"],
            "cache_prompt": true
        });

        let url = format!("http://127.0.0.1:{port}/completion");
        let resp: serde_json::Value =
            self.http.post(&url).json(&body).send().await?.error_for_status()?.json().await?;
        let content = resp["content"].as_str().unwrap_or_default().trim().to_string();
        if content.is_empty() {
            anyhow::bail!("SLM returned empty content");
        }
        // Do not embed `content` in the error: grammar-constrained output is
        // document-derived and this string is logged. Keep it value-free.
        let out: SlmOutput = serde_json::from_str(&content)
            .map_err(|e| anyhow::anyhow!("SLM output failed JSON parse despite grammar: {e}"))?;
        Ok(out)
    }

    pub fn shutdown(&self) {
        for slot in [&self.primary, &self.escalation] {
            if let Some(mut c) = slot.lock().unwrap().take() {
                let _ = c.kill();
            }
        }
    }
}

impl Drop for SlmLane {
    fn drop(&mut self) {
        self.shutdown();
    }
}
