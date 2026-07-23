//! In-app, one-time downloader for BackLog's model bundle.
//!
//! Mirrors `models/download_models.py`'s repo ids, filenames, and TOFU-then-
//! verify lock behavior so a non-technical user never opens a terminal to
//! finish setup. This is BackLog's one deliberate exception to "offline at
//! inference time": a single outbound fetch to public Hugging Face model
//! repos, run once from Settings, never during document processing.
//!
//! Layout on disk mirrors `models/download_models.py`'s targets one-to-one:
//! the two Qwen GGUFs land wherever `Config::slm_primary_gguf` /
//! `slm_escalation_gguf` point (normally `app_data/models/…`, see
//! [`resolve_configured_model_path`]), and the gliclass/granite files land
//! under `<models_dir>/<target>` where `<models_dir>` is exactly what
//! `sidecar.rs` injects into the convertd sidecar's `BACKLOG_MODELS_DIR`.

use crate::AppState;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// Default basename for the primary-tier GGUF, used when a configured path
/// is unset (never downloaded/configured yet).
pub const PRIMARY_GGUF_NAME: &str = "Qwen3-0.6B-Q8_0.gguf";
/// Default basename for the escalation-tier GGUF.
pub const ESCALATION_GGUF_NAME: &str = "Qwen3-1.7B-Q8_0.gguf";

const HF_HOST: &str = "https://huggingface.co";

/// One file BackLog's runtime bundle needs, mirroring a `ModelSpec` entry in
/// `models/download_models.py`. `repo` + `hf_path` address the file on the
/// Hub (`{HF_HOST}/{repo}/resolve/main/{hf_path}`); `target` is the
/// project-relative destination `download_models.py` and `models.lock.json`
/// use, and doubles as this downloader's lock key so the two tools describe
/// the same bundle in the same vocabulary.
///
/// `download_models.py` fetches the gliclass/granite entries with
/// `snapshot_download`, which mirrors the *entire* upstream repo (READMEs,
/// `.gitattributes`, and — for granite — a `pytorch_model.bin` that
/// duplicates `model.safetensors` in a different format). This list instead
/// curates just the files `sidecar/convertd.py`'s `_gliclass`/`_granite`
/// loaders actually open, which is what keeps a fresh install close to the
/// size the UI quotes rather than paying for docs and a redundant weights
/// copy. Verified against the live repo trees on 2026-07-22.
pub struct ModelFile {
    pub repo: &'static str,
    pub hf_path: &'static str,
    pub target: &'static str,
    /// Approximate size in bytes. Used for the pre-verification progress
    /// denominator and the overall byte-percent when a server response omits
    /// `Content-Length`; never trusted for correctness (the real byte count
    /// and SHA-256 always come from the stream itself).
    pub size_hint: u64,
}

pub const MODEL_FILES: &[ModelFile] = &[
    // Apache-2.0 Qwen3 GGUFs served by llama.cpp.
    ModelFile {
        repo: "Qwen/Qwen3-0.6B-GGUF",
        hf_path: PRIMARY_GGUF_NAME,
        target: PRIMARY_GGUF_NAME,
        size_hint: 639_446_688,
    },
    ModelFile {
        repo: "Qwen/Qwen3-1.7B-GGUF",
        hf_path: ESCALATION_GGUF_NAME,
        target: ESCALATION_GGUF_NAME,
        size_hint: 1_834_426_016,
    },
    // Zero-shot document classification (knowledgator/gliclass-base-v3.0).
    ModelFile {
        repo: "knowledgator/gliclass-base-v3.0",
        hf_path: "config.json",
        target: "gliclass-base-v3.0/config.json",
        size_hint: 3_493,
    },
    ModelFile {
        repo: "knowledgator/gliclass-base-v3.0",
        hf_path: "added_tokens.json",
        target: "gliclass-base-v3.0/added_tokens.json",
        size_hint: 67,
    },
    ModelFile {
        repo: "knowledgator/gliclass-base-v3.0",
        hf_path: "special_tokens_map.json",
        target: "gliclass-base-v3.0/special_tokens_map.json",
        size_hint: 970,
    },
    ModelFile {
        repo: "knowledgator/gliclass-base-v3.0",
        hf_path: "tokenizer_config.json",
        target: "gliclass-base-v3.0/tokenizer_config.json",
        size_hint: 1_692,
    },
    ModelFile {
        repo: "knowledgator/gliclass-base-v3.0",
        hf_path: "spm.model",
        target: "gliclass-base-v3.0/spm.model",
        size_hint: 2_464_616,
    },
    ModelFile {
        repo: "knowledgator/gliclass-base-v3.0",
        hf_path: "tokenizer.json",
        target: "gliclass-base-v3.0/tokenizer.json",
        size_hint: 8_649_234,
    },
    ModelFile {
        repo: "knowledgator/gliclass-base-v3.0",
        hf_path: "model.safetensors",
        target: "gliclass-base-v3.0/model.safetensors",
        size_hint: 746_211_800,
    },
    // Salience embeddings (ibm-granite/granite-embedding-small-english-r2).
    ModelFile {
        repo: "ibm-granite/granite-embedding-small-english-r2",
        hf_path: "config.json",
        target: "granite-embedding-small-english-r2/config.json",
        size_hint: 1_315,
    },
    ModelFile {
        repo: "ibm-granite/granite-embedding-small-english-r2",
        hf_path: "modules.json",
        target: "granite-embedding-small-english-r2/modules.json",
        size_hint: 230,
    },
    ModelFile {
        repo: "ibm-granite/granite-embedding-small-english-r2",
        hf_path: "sentence_bert_config.json",
        target: "granite-embedding-small-english-r2/sentence_bert_config.json",
        size_hint: 55,
    },
    ModelFile {
        repo: "ibm-granite/granite-embedding-small-english-r2",
        hf_path: "special_tokens_map.json",
        target: "granite-embedding-small-english-r2/special_tokens_map.json",
        size_hint: 694,
    },
    ModelFile {
        repo: "ibm-granite/granite-embedding-small-english-r2",
        hf_path: "tokenizer_config.json",
        target: "granite-embedding-small-english-r2/tokenizer_config.json",
        size_hint: 20_836,
    },
    ModelFile {
        repo: "ibm-granite/granite-embedding-small-english-r2",
        hf_path: "tokenizer.json",
        target: "granite-embedding-small-english-r2/tokenizer.json",
        size_hint: 3_583_228,
    },
    ModelFile {
        repo: "ibm-granite/granite-embedding-small-english-r2",
        hf_path: "1_Pooling/config.json",
        target: "granite-embedding-small-english-r2/1_Pooling/config.json",
        size_hint: 191,
    },
    ModelFile {
        repo: "ibm-granite/granite-embedding-small-english-r2",
        hf_path: "model.safetensors",
        target: "granite-embedding-small-english-r2/model.safetensors",
        size_hint: 95_332_048,
    },
];

/// Builds a Hugging Face `resolve/main` URL for one [`ModelFile`]. HF resolve
/// URLs 302 to a CDN; that's handled by `reqwest::Client`'s default redirect
/// policy (follow, capped at 10 hops) with no extra configuration.
pub fn download_url(repo: &str, hf_path: &str) -> String {
    let encoded_segments: Vec<String> = hf_path.split('/').map(percent_encode_segment).collect();
    format!("{HF_HOST}/{repo}/resolve/main/{}", encoded_segments.join("/"))
}

/// Percent-encodes one path segment. None of the pinned files need this
/// today (plain ASCII names), but it keeps `download_url` correct if a
/// future spec entry has a space or other reserved character, without
/// pulling in a dedicated URL-encoding crate for one call site.
fn percent_encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Resolves a configured model path (as stored in `Config`) to an absolute
/// path under `models_dir`. Absolute paths are returned unchanged — a user
/// who explicitly pointed Settings at a pre-existing GGUF elsewhere is never
/// silently redirected. Empty or relative paths (the shipped defaults, e.g.
/// `"models/Qwen3-0.6B-Q8_0.gguf"`, or anything left blank) resolve to
/// `models_dir/<basename>`, so an installed app — whose working directory
/// bears no relation to `models_dir` — always finds them.
pub fn resolve_configured_model_path(models_dir: &Path, configured: &Path, default_name: &str) -> PathBuf {
    if configured.as_os_str().is_empty() {
        return models_dir.join(default_name);
    }
    if configured.is_absolute() {
        return configured.to_path_buf();
    }
    match configured.file_name() {
        Some(name) => models_dir.join(name),
        None => models_dir.join(default_name),
    }
}

/// `app_data_dir()/models` — the persistent, installed-app-safe home for the
/// whole runtime bundle. Also the value injected into the convertd sidecar's
/// `BACKLOG_MODELS_DIR` (see `sidecar.rs::Sidecar::with_models_dir`).
pub fn resolve_models_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("models"))
        .unwrap_or_else(|_| PathBuf::from("models"))
}

/// One [`ModelFile`] resolved to a concrete destination on this machine.
/// Pure data + pure construction (see [`download_targets`]) so both are unit
/// testable without touching disk or Tauri's `AppHandle`.
struct DownloadTarget {
    repo: &'static str,
    hf_path: &'static str,
    /// Lock key — always [`ModelFile::target`], never the resolved `dest`,
    /// so `models.lock.json` stays stable and comparable to
    /// `download_models.py`'s regardless of where a user's custom GGUF path
    /// physically lives.
    key: &'static str,
    dest: PathBuf,
    size_hint: u64,
}

/// Maps every [`MODEL_FILES`] entry onto a concrete destination: the two
/// GGUFs go wherever the running config's `slm_primary_gguf` /
/// `slm_escalation_gguf` currently resolve to (so a user's custom Settings
/// path is honored), everything else goes under `<models_dir>/<target>`.
fn download_targets(models_dir: &Path, primary_gguf: &Path, escalation_gguf: &Path) -> Vec<DownloadTarget> {
    MODEL_FILES
        .iter()
        .map(|f| {
            let dest = if f.target == PRIMARY_GGUF_NAME {
                primary_gguf.to_path_buf()
            } else if f.target == ESCALATION_GGUF_NAME {
                escalation_gguf.to_path_buf()
            } else {
                models_dir.join(f.target)
            };
            DownloadTarget { repo: f.repo, hf_path: f.hf_path, key: f.target, dest, size_hint: f.size_hint }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    /// The spec's `target` key of the file currently in flight (e.g.
    /// `"granite-embedding-small-english-r2/model.safetensors"`) — stable
    /// and unambiguous even though several files share a bare filename like
    /// `config.json` across the gliclass/granite directories.
    pub current_file: String,
    pub file_bytes_done: u64,
    pub file_bytes_total: u64,
    /// Files fully verified before this one started. Ranges `0..files_total`
    /// while `current_file` is in flight; the frontend can show
    /// `files_done + 1` of `files_total` for a 1-based counter.
    pub files_done: usize,
    pub files_total: usize,
    /// 0-100 across the whole bundle, weighted by byte size (not file
    /// count), so the 1.8 GB escalation GGUF moves the bar proportionally
    /// more than a handful of kilobyte-sized tokenizer configs.
    pub overall_percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadDone {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

const PROGRESS_THROTTLE: Duration = Duration::from_millis(200);

fn part_path_for(dest: &Path) -> PathBuf {
    let mut os_string = dest.as_os_str().to_os_string();
    os_string.push(".part");
    PathBuf::from(os_string)
}

fn load_lock(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn write_lock(path: &Path, lock: &BTreeMap<String, String>) -> Result<(), String> {
    let json = serde_json::to_string_pretty(lock).map_err(|e| format!("could not serialize models.lock.json: {e}"))?;
    std::fs::write(path, json + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

#[allow(clippy::too_many_arguments)]
fn emit_progress(
    app: &AppHandle,
    target: &DownloadTarget,
    file_bytes_done: u64,
    file_bytes_total: u64,
    files_done: usize,
    files_total: usize,
    overall_bytes_done: u64,
    grand_total_bytes: u64,
) {
    let overall_percent = if grand_total_bytes > 0 {
        (overall_bytes_done as f64 / grand_total_bytes as f64 * 100.0).clamp(0.0, 100.0)
    } else {
        100.0
    };
    let _ = app.emit(
        "model-download-progress",
        &DownloadProgress {
            current_file: target.key.to_string(),
            file_bytes_done,
            file_bytes_total,
            files_done,
            files_total,
            overall_percent,
        },
    );
}

/// Streams one file to `<dest>.part`, then atomically renames it into place.
/// A `.part` left behind by a crash or a prior failed attempt is never
/// trusted or resumed — it's deleted up front and the file starts over, per
/// the "cancel-safe-ish" contract: no partial-byte-range bookkeeping to get
/// wrong, just a clean restart.
async fn download_one(
    client: &reqwest::Client,
    app: &AppHandle,
    target: &DownloadTarget,
    files_done: usize,
    files_total: usize,
    bytes_done_before_this_file: u64,
    grand_total_bytes: u64,
) -> Result<(u64, String), String> {
    use futures_util::StreamExt;

    let url = download_url(target.repo, target.hf_path);
    if let Some(parent) = target.dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let part_path = part_path_for(&target.dest);
    let _ = std::fs::remove_file(&part_path); // never trust a leftover partial

    let response = client
        .get(&url)
        .timeout(Duration::from_secs(30 * 60))
        .send()
        .await
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("GET {url} returned HTTP {}", response.status()));
    }
    let content_length = response.content_length().unwrap_or(target.size_hint);

    let mut file = std::fs::File::create(&part_path).map_err(|e| format!("{}: {e}", part_path.display()))?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut last_emit = Instant::now() - PROGRESS_THROTTLE;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download of {url} interrupted: {e}"))?;
        file.write_all(&chunk).map_err(|e| format!("{}: {e}", part_path.display()))?;
        hasher.update(&chunk);
        written += chunk.len() as u64;

        if last_emit.elapsed() >= PROGRESS_THROTTLE {
            last_emit = Instant::now();
            emit_progress(
                app,
                target,
                written,
                content_length,
                files_done,
                files_total,
                bytes_done_before_this_file + written,
                grand_total_bytes,
            );
        }
    }
    file.sync_all().map_err(|e| format!("{}: {e}", part_path.display()))?;
    drop(file);

    // Final tick for this file regardless of the throttle, so the bar always
    // visibly reaches 100% for it even if the last chunk landed inside the
    // throttle window (or the whole file was one chunk).
    emit_progress(
        app,
        target,
        written,
        written,
        files_done,
        files_total,
        bytes_done_before_this_file + written,
        grand_total_bytes,
    );

    std::fs::rename(&part_path, &target.dest).map_err(|e| format!("{}: {e}", target.dest.display()))?;
    Ok((written, hex::encode(hasher.finalize())))
}

/// Ensures one target exists and is hash-verified, downloading it if
/// necessary. Returns the number of bytes it contributes to the running
/// overall-progress total (its full size either way, whether just downloaded
/// or already present).
#[allow(clippy::too_many_arguments)]
async fn ensure_file(
    app: &AppHandle,
    client: &reqwest::Client,
    target: &DownloadTarget,
    lock: &mut BTreeMap<String, String>,
    lock_path: &Path,
    files_done: usize,
    files_total: usize,
    bytes_done_before: u64,
    grand_total_bytes: u64,
) -> Result<u64, String> {
    let existing_len = std::fs::metadata(&target.dest).map(|m| m.len()).unwrap_or(0);
    if existing_len > 0 {
        let actual = crate::pipeline::hash_file(&target.dest).map_err(|e| e.to_string())?;
        match lock.get(target.key) {
            Some(expected) if expected == &actual => {
                // Already present and verified — still tick progress so the
                // bar and file counter advance past it.
                emit_progress(app, target, existing_len, existing_len, files_done, files_total, bytes_done_before + existing_len, grand_total_bytes);
                return Ok(existing_len);
            }
            Some(expected) => {
                // Mirrors download_models.py: never silently overwrite a
                // file that no longer matches its locked hash.
                return Err(format!(
                    "{} exists but does not match the locked hash (locked {expected}, found {actual}); \
                     delete it manually before retrying",
                    target.dest.display()
                ));
            }
            None => {
                // Trust-on-first-use: an already-present file with no lock
                // entry yet (e.g. a user-supplied path) is recorded, not
                // re-downloaded.
                lock.insert(target.key.to_string(), actual);
                write_lock(lock_path, lock)?;
                emit_progress(app, target, existing_len, existing_len, files_done, files_total, bytes_done_before + existing_len, grand_total_bytes);
                return Ok(existing_len);
            }
        }
    }

    let (written, digest) = download_one(client, app, target, files_done, files_total, bytes_done_before, grand_total_bytes).await?;

    match lock.get(target.key) {
        Some(expected) if expected != &digest => {
            let _ = std::fs::remove_file(&target.dest);
            return Err(format!(
                "downloaded {} but its hash did not match the locked value (locked {expected}, got {digest}); \
                 the Hugging Face copy may have changed since this bundle was locked",
                target.dest.display()
            ));
        }
        Some(_) => {}
        None => {
            lock.insert(target.key.to_string(), digest);
            write_lock(lock_path, lock)?;
        }
    }

    Ok(written)
}

/// Downloads BackLog's full model bundle, skipping anything already present
/// and hash-valid. Progress streams out via the `model-download-progress`
/// event; a terminal `model-download-done` event always follows, carrying
/// `{ ok: false, error }` on failure instead of relying solely on the
/// command's `Result` (which the frontend also sees, but a page that missed
/// the promise rejection — e.g. because it navigated away and back — can
/// still resync from the event).
#[tauri::command]
pub async fn download_models(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let models_dir = resolve_models_dir(&app);
    std::fs::create_dir_all(&models_dir).map_err(|e| format!("could not create {}: {e}", models_dir.display()))?;

    let (primary_gguf, escalation_gguf) = {
        let cfg = state.cfg.lock().unwrap();
        (cfg.slm_primary_gguf.clone(), cfg.slm_escalation_gguf.clone())
    };
    let targets = download_targets(&models_dir, &primary_gguf, &escalation_gguf);
    let grand_total_bytes: u64 = targets.iter().map(|t| t.size_hint).sum();
    let files_total = targets.len();

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("could not build HTTP client: {e}"))?;

    let lock_path = models_dir.join("models.lock.json");
    let mut lock = load_lock(&lock_path);

    let result: Result<(), String> = async {
        let mut bytes_done: u64 = 0;
        for (files_done, target) in targets.iter().enumerate() {
            let written = ensure_file(&app, &client, target, &mut lock, &lock_path, files_done, files_total, bytes_done, grand_total_bytes).await?;
            bytes_done += written;
        }
        Ok(())
    }
    .await;

    let _ = app.emit(
        "model-download-done",
        &DownloadDone { ok: result.is_ok(), error: result.as_ref().err().cloned() },
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_url_joins_repo_and_path() {
        assert_eq!(
            download_url("Qwen/Qwen3-0.6B-GGUF", "Qwen3-0.6B-Q8_0.gguf"),
            "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q8_0.gguf"
        );
    }

    #[test]
    fn download_url_preserves_nested_hf_paths() {
        assert_eq!(
            download_url("ibm-granite/granite-embedding-small-english-r2", "1_Pooling/config.json"),
            "https://huggingface.co/ibm-granite/granite-embedding-small-english-r2/resolve/main/1_Pooling/config.json"
        );
    }

    #[test]
    fn download_url_percent_encodes_reserved_characters() {
        assert_eq!(download_url("owner/repo", "a file.bin"), "https://huggingface.co/owner/repo/resolve/main/a%20file.bin");
    }

    #[test]
    fn every_spec_entry_resolves_to_a_valid_download_url() {
        // Cheap sanity net for future MODEL_FILES edits: every entry must
        // still produce a well-formed https URL under the HF host.
        for file in MODEL_FILES {
            let url = download_url(file.repo, file.hf_path);
            assert!(url.starts_with("https://huggingface.co/"), "{url}");
            assert!(url.ends_with(&format!("/resolve/main/{}", file.hf_path)), "{url}");
        }
    }

    #[test]
    fn model_files_has_no_duplicate_targets() {
        let mut seen = std::collections::HashSet::new();
        for file in MODEL_FILES {
            assert!(seen.insert(file.target), "duplicate target: {}", file.target);
        }
    }

    #[test]
    fn resolve_configured_model_path_leaves_absolute_paths_untouched() {
        // A hardcoded "/custom/..." literal is not `is_absolute()` on
        // Windows (no drive prefix) — use a real tempdir so this exercises
        // a genuinely OS-absolute path on every platform.
        let models_dir = Path::new("/app-data/models");
        let custom_dir = tempfile::tempdir().unwrap();
        let configured = custom_dir.path().join("Qwen3-0.6B-Q8_0.gguf");
        assert!(configured.is_absolute());
        assert_eq!(resolve_configured_model_path(models_dir, &configured, PRIMARY_GGUF_NAME), configured);
    }

    #[test]
    fn resolve_configured_model_path_rehomes_relative_default() {
        let models_dir = Path::new("/app-data/models");
        let configured = Path::new("models/Qwen3-0.6B-Q8_0.gguf");
        assert_eq!(
            resolve_configured_model_path(models_dir, configured, PRIMARY_GGUF_NAME),
            Path::new("/app-data/models/Qwen3-0.6B-Q8_0.gguf")
        );
    }

    #[test]
    fn resolve_configured_model_path_fills_in_unset_paths() {
        let models_dir = Path::new("/app-data/models");
        assert_eq!(
            resolve_configured_model_path(models_dir, Path::new(""), ESCALATION_GGUF_NAME),
            Path::new("/app-data/models/Qwen3-1.7B-Q8_0.gguf")
        );
    }

    #[test]
    fn resolve_configured_model_path_is_idempotent() {
        // Safe to call unconditionally on every startup (lib.rs does).
        let models_dir = Path::new("/app-data/models");
        let once = resolve_configured_model_path(models_dir, Path::new("models/x.gguf"), PRIMARY_GGUF_NAME);
        let twice = resolve_configured_model_path(models_dir, &once, PRIMARY_GGUF_NAME);
        assert_eq!(once, twice);
    }

    #[test]
    fn download_targets_routes_gguf_files_to_the_configured_config_paths() {
        let models_dir = Path::new("/app-data/models");
        let primary = Path::new("/custom/primary.gguf");
        let escalation = Path::new("/app-data/models/Qwen3-1.7B-Q8_0.gguf");
        let targets = download_targets(models_dir, primary, escalation);

        let primary_target = targets.iter().find(|t| t.key == PRIMARY_GGUF_NAME).unwrap();
        assert_eq!(primary_target.dest, primary);
        let escalation_target = targets.iter().find(|t| t.key == ESCALATION_GGUF_NAME).unwrap();
        assert_eq!(escalation_target.dest, escalation);
    }

    #[test]
    fn download_targets_routes_directory_files_under_models_dir() {
        let models_dir = Path::new("/app-data/models");
        let targets = download_targets(models_dir, Path::new("/x/p.gguf"), Path::new("/x/e.gguf"));
        let gliclass_config = targets.iter().find(|t| t.key == "gliclass-base-v3.0/config.json").unwrap();
        assert_eq!(gliclass_config.dest, models_dir.join("gliclass-base-v3.0/config.json"));
        let pooling = targets
            .iter()
            .find(|t| t.key == "granite-embedding-small-english-r2/1_Pooling/config.json")
            .unwrap();
        assert_eq!(pooling.dest, models_dir.join("granite-embedding-small-english-r2/1_Pooling/config.json"));
    }

    #[test]
    fn download_targets_covers_every_spec_entry_exactly_once() {
        let targets = download_targets(Path::new("/m"), Path::new("/p.gguf"), Path::new("/e.gguf"));
        assert_eq!(targets.len(), MODEL_FILES.len());
    }

    #[test]
    fn part_path_appends_suffix_without_losing_the_original_name() {
        let dest = Path::new("/app-data/models/gliclass-base-v3.0/model.safetensors");
        assert_eq!(part_path_for(dest), Path::new("/app-data/models/gliclass-base-v3.0/model.safetensors.part"));
    }

    #[test]
    fn lock_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.lock.json");
        let mut lock = BTreeMap::new();
        lock.insert("Qwen3-0.6B-Q8_0.gguf".to_string(), "a".repeat(64));
        write_lock(&path, &lock).unwrap();
        assert_eq!(load_lock(&path), lock);
    }

    #[test]
    fn load_lock_defaults_to_empty_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_lock(&dir.path().join("nope.json")).is_empty());
    }
}
