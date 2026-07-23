//! Local structured-output naming lane backed by llama.cpp.
//!
//! The primary Qwen3-0.6B server starts on demand. The Qwen3-1.7B escalation
//! server starts only after a rejected primary attempt and remains resident for
//! the batch. Both bind to loopback and use the model's embedded chat template
//! through llama.cpp's OpenAI-compatible chat-completions endpoint.
//!
//! Qwen3 and its GGUF conversions are Apache-2.0; this replaces the prior
//! Liquid-licensed LFM2.5 lane so the app can be redistributed without a
//! non-standard model license.

use crate::checker::SlmOutput;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

pub struct SlmLane {
    // Retained as a compatibility input while older installations still bundle
    // the GBNF resource. Current chat requests use JSON Schema directly.
    _fallback_grammar: String,
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
            _fallback_grammar: grammar,
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
                .expect("localhost HTTP client"),
        }
    }

    fn spawn_server(&self, gguf: &Path, port: u16) -> anyhow::Result<Child> {
        anyhow::ensure!(gguf.is_file(), "GGUF model not found: {}", gguf.display());
        let mut command = Command::new(&self.llama_server_exe);
        command
            .args([
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
                // Required so llama-server renders Qwen3's embedded chat
                // template for the /v1/chat/completions endpoint below.
                "--jinja",
                "--no-webui",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        Ok(command.spawn()?)
    }

    async fn ensure_up(&self, tier: Tier) -> anyhow::Result<u16> {
        let (slot, gguf, port) = match tier {
            Tier::Primary => (&self.primary, &self.primary_gguf, self.primary_port),
            Tier::Escalation => (&self.escalation, &self.escalation_gguf, self.escalation_port),
        };
        {
            let mut guard = slot.lock().unwrap();
            let must_spawn = match guard.as_mut() {
                None => true,
                Some(child) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
            };
            if must_spawn {
                *guard = Some(self.spawn_server(gguf, port)?);
            }
        }

        let health_url = format!("http://127.0.0.1:{port}/health");
        for _ in 0..120 {
            if let Ok(response) = self.http.get(&health_url).send().await {
                if response.status().is_success() {
                    return Ok(port);
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        anyhow::bail!("llama-server on port {port} never became healthy")
    }

    fn response_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["date", "date_source", "subject", "description"],
            "properties": {
                "date": {
                    "type": "string",
                    "pattern": "^(?:none|[12][0-9]{3}-[0-9]{2}-[0-9]{2})$"
                },
                "date_source": {
                    "type": "string",
                    "enum": ["document", "metadata", "none"]
                },
                "subject": {
                    "type": "string",
                    "minLength": 8,
                    "maxLength": 80
                },
                "description": {
                    "type": "string",
                    "minLength": 15,
                    "maxLength": 200
                }
            }
        })
    }

    fn request_body(system: &str, evidence: &str) -> Value {
        json!({
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": evidence}
            ],
            "temperature": 0.0,
            "max_tokens": 220,
            "stream": false,
            "cache_prompt": false,
            "chat_template_kwargs": {"enable_thinking": false},
            "response_format": {
                "type": "json_schema",
                "schema": Self::response_schema()
            }
        })
    }

    fn chat_content(response: &Value) -> anyhow::Result<String> {
        let content = &response["choices"][0]["message"]["content"];
        if let Some(text) = content.as_str() {
            return Ok(text.trim().to_string());
        }
        if let Some(parts) = content.as_array() {
            let joined = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            if !joined.trim().is_empty() {
                return Ok(joined.trim().to_string());
            }
        }
        anyhow::bail!("llama-server chat response did not contain assistant content")
    }

    fn json_object(text: &str) -> anyhow::Result<&str> {
        let trimmed = text.trim();
        let start = trimmed
            .find('{')
            .ok_or_else(|| anyhow::anyhow!("assistant content contained no JSON object"))?;
        let end = trimmed
            .rfind('}')
            .ok_or_else(|| anyhow::anyhow!("assistant content contained incomplete JSON"))?;
        anyhow::ensure!(end >= start, "assistant JSON boundaries were invalid");
        Ok(&trimmed[start..=end])
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
             Do not reveal reasoning. Return only the requested JSON object.\n\
             Rules:\n\
             - date: choose the document's own date in YYYY-MM-DD only from dates visible in the evidence. If no date is visible, use none.\n\
             - date_source: document when visible in document text, metadata when visible only in file metadata, or none when there is no date.\n\
             - subject: 3 to 8 specific words naming the document type and key party or matter. Never use generic names such as Document or Scan.\n\
             - description: exactly one sentence, 15 to 200 characters, adding useful information beyond the subject.\n\
             Never invent dates, parties, or facts."
        );
        if let Some(violation) = violation_note {
            system.push_str(&format!(
                "\nA prior proposal was rejected by the deterministic validator: {violation}. Correct that exact problem."
            ));
        }

        let body = Self::request_body(&system, evidence);
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let response: Value = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let content = Self::chat_content(&response)?;
        let object = Self::json_object(&content)?;
        serde_json::from_str(object).map_err(|error| {
            anyhow::anyhow!(
                "SLM output failed JSON parsing despite schema constraint: {error}; raw: {content}"
            )
        })
    }

    pub fn shutdown(&self) {
        for slot in [&self.primary, &self.escalation] {
            if let Some(mut child) = slot.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

impl Drop for SlmLane {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_chat_template_and_schema_without_raw_prompt() {
        let body = SlmLane::request_body("system", "evidence");
        assert!(body.get("prompt").is_none());
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(
            body["response_format"]["schema"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn extracts_content_from_openai_style_response() {
        let response = json!({
            "choices": [{"message": {"content": "{\"date\":\"none\"}"}}]
        });
        assert_eq!(
            SlmLane::chat_content(&response).unwrap(),
            "{\"date\":\"none\"}"
        );
    }

    #[test]
    fn isolates_json_from_template_noise() {
        let value = "<think></think>\n```json\n{\"date\":\"none\"}\n```";
        assert_eq!(SlmLane::json_object(value).unwrap(), "{\"date\":\"none\"}");
    }
}
