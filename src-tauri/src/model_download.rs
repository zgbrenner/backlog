//! In-app, one-time downloader for BackLog's model bundle.
//!
//! Mirrors `models/download_models.py`'s repo ids and filenames so a
//! non-technical user never opens a terminal to finish setup. This is
//! BackLog's one deliberate exception to "offline at inference time": a single
//! outbound fetch to public Hugging Face model repos, run once from Settings,
//! never during document processing.
//!
//! Layout on disk mirrors `models/download_models.py`'s targets one-to-one:
//! the two Qwen GGUFs land wherever `Config::slm_primary_gguf` /
//! `slm_escalation_gguf` point (normally `app_data/models/…`, see
//! [`resolve_configured_model_path`]). That's the whole bundle: the slim,
//! torch-free sidecar profile ships without the gliclass/granite naming
//! enhancements (see `sidecar/convertd.py`'s `_gliclass`/`_granite` loaders
//! and `docs/DEPENDENCY_COMPATIBILITY.md`), so there are no torch-only model
//! snapshots left to fetch or verify here.
//!
//! ## Integrity
//!
//! Each entry carries a compile-time [`ModelFile::expected_sha256`]. Nothing
//! is ever renamed into place, or accepted as already-present, unless its
//! digest matches that constant exactly. There is deliberately no
//! trust-on-first-use path: the digests are what a 2.4 GB blob that
//! llama.cpp mmaps and parses is checked against, and a lock file written by
//! the same run that fetched the bytes cannot vouch for them. The committed
//! `models/models.lock.json` records the same two digests for the staging
//! script and the release checklist; a test below keeps the two in step.

use crate::AppState;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
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
/// use, and doubles as this downloader's provenance key so the two tools
/// describe the same bundle in the same vocabulary.
///
/// The slim, torch-free sidecar profile has no gliclass/granite model
/// snapshots to fetch (see `sidecar/convertd.py`'s `_gliclass`/`_granite`
/// loaders, which degrade to deterministic fallbacks when those libraries or
/// snapshots are absent), so this list is just the two Apache-2.0 Qwen3
/// GGUFs. Verified against the live repo trees on 2026-07-22.
pub struct ModelFile {
    pub repo: &'static str,
    pub hf_path: &'static str,
    pub target: &'static str,
    /// SHA-256 the downloaded bytes must hash to, pinned at compile time.
    /// Taken from the Hub's own git-LFS object id for the file, which is the
    /// SHA-256 of its contents, and mirrored in `models/models.lock.json`.
    pub expected_sha256: &'static str,
    /// Exact size in bytes as published by the Hub. Used for the progress
    /// denominator before a response arrives and for the free-space
    /// precheck; never trusted for correctness (the SHA-256 above is the
    /// only thing that decides whether a file is accepted).
    pub size_hint: u64,
}

pub const MODEL_FILES: &[ModelFile] = &[
    // Apache-2.0 Qwen3 GGUFs served by llama.cpp. This is the whole bundle
    // on the slim, torch-free sidecar profile -- no gliclass/granite
    // snapshots to fetch (see the module doc comment above).
    ModelFile {
        repo: "Qwen/Qwen3-0.6B-GGUF",
        hf_path: PRIMARY_GGUF_NAME,
        target: PRIMARY_GGUF_NAME,
        expected_sha256: "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031",
        size_hint: 639_446_688,
    },
    ModelFile {
        repo: "Qwen/Qwen3-1.7B-GGUF",
        hf_path: ESCALATION_GGUF_NAME,
        target: ESCALATION_GGUF_NAME,
        expected_sha256: "061b54daade076b5d3362dac252678d17da8c68f07560be70818cace6590cb1a",
        size_hint: 1_834_426_016,
    },
];

/// Builds a Hugging Face `resolve/main` URL for one [`ModelFile`]. HF resolve
/// URLs 302 to a CDN, so the client keeps redirects on — but capped, and
/// HTTPS-only, so a redirect can never walk the transfer down to plaintext.
pub fn download_url(repo: &str, hf_path: &str) -> String {
    let encoded_segments: Vec<String> = hf_path.split('/').map(percent_encode_segment).collect();
    format!(
        "{HF_HOST}/{repo}/resolve/main/{}",
        encoded_segments.join("/")
    )
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
pub fn resolve_configured_model_path(
    models_dir: &Path,
    configured: &Path,
    default_name: &str,
) -> PathBuf {
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
    /// Provenance key — always [`ModelFile::target`], never the resolved
    /// `dest`, so `models.lock.json` stays stable and comparable to
    /// `download_models.py`'s regardless of where a user's custom GGUF path
    /// physically lives.
    key: &'static str,
    /// Where the bytes come from. A field rather than a call to
    /// [`download_url`] at use time so the streaming path can be pointed at a
    /// loopback test server without a live Hub round trip.
    url: String,
    expected_sha256: &'static str,
    dest: PathBuf,
    size_hint: u64,
}

/// Maps every [`MODEL_FILES`] entry onto a concrete destination: distinct
/// configured GGUF paths are honored, while a legacy/collapsed pair is
/// separated into the canonical filenames under `models_dir` so one download
/// can never overwrite the other.
fn download_targets(
    models_dir: &Path,
    primary_gguf: &Path,
    escalation_gguf: &Path,
) -> Vec<DownloadTarget> {
    let destinations_collide = primary_gguf == escalation_gguf;
    MODEL_FILES
        .iter()
        .map(|f| {
            let dest = if f.target == PRIMARY_GGUF_NAME {
                if destinations_collide {
                    models_dir.join(PRIMARY_GGUF_NAME)
                } else {
                    primary_gguf.to_path_buf()
                }
            } else if f.target == ESCALATION_GGUF_NAME {
                if destinations_collide {
                    models_dir.join(ESCALATION_GGUF_NAME)
                } else {
                    escalation_gguf.to_path_buf()
                }
            } else {
                models_dir.join(f.target)
            };
            DownloadTarget {
                key: f.target,
                url: download_url(f.repo, f.hf_path),
                expected_sha256: f.expected_sha256,
                dest,
                size_hint: f.size_hint,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    /// The spec's `target` key of the file currently in flight (e.g.
    /// `"Qwen3-1.7B-Q8_0.gguf"`) — stable and unambiguous even if a future
    /// bundle entry has a bare filename shared across directories.
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
    /// more than the 0.6 GB primary one.
    pub overall_percent: f64,
    /// Recent throughput over a rolling window, `null` until enough of the
    /// stream has arrived to mean anything. A 2.4 GB transfer with no rate
    /// and no ETA is indistinguishable from a stalled one.
    pub bytes_per_sec: Option<f64>,
    /// Seconds remaining for the whole bundle at the current rate.
    pub eta_secs: Option<u64>,
}

/// Terminal event for one `download_models` run. Carries enough structure for
/// the frontend to re-render the outcome later (see [`model_download_status`])
/// instead of depending on a toast the user has to be looking at.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadDone {
    pub ok: bool,
    /// True only for an operator-requested stop, which is not a failure and
    /// must not be reported as one.
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// RFC 3339 timestamp, so a failure the user walked away from still says
    /// when it happened.
    pub finished_at: String,
}

const PROGRESS_THROTTLE: Duration = Duration::from_millis(200);
/// Rolling window for the throughput estimate. Long enough to ride out a
/// stalled TCP window, short enough to react when an office link degrades.
const RATE_WINDOW: Duration = Duration::from_secs(5);
/// Headroom demanded on top of the bundle size before the first byte is
/// fetched: the OS, OneDrive and the eventual rename all need room, and
/// finding out after a 2.4 GB download is the worst possible time.
const FREE_SPACE_MARGIN: u64 = 512 * 1024 * 1024;

/// Set by [`cancel_model_download`], cleared at the start of every run.
/// Module-level rather than in `AppState` because the download is a
/// singleton operation the user starts from one button.
static CANCEL: AtomicBool = AtomicBool::new(false);

/// Last terminal outcome, so a user who navigated away from Settings (or
/// stepped away for the six seconds a toast lives) can still be told what
/// happened.
static LAST_OUTCOME: Mutex<Option<DownloadDone>> = Mutex::new(None);

fn part_path_for(dest: &Path) -> PathBuf {
    let mut os_string = dest.as_os_str().to_os_string();
    os_string.push(".part");
    PathBuf::from(os_string)
}

/// Record what is installed, for the release checklist and support triage.
/// Purely descriptive: it is written *after* the pinned digest has already
/// decided the file is good, and it is never read back as a trust anchor.
fn write_lock(path: &Path, lock: &BTreeMap<String, String>) -> Result<(), String> {
    let json = serde_json::to_string_pretty(lock)
        .map_err(|e| format!("could not serialize models.lock.json: {e}"))?;
    std::fs::write(path, json + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Throughput
// ---------------------------------------------------------------------------

/// Rolling-window throughput estimate over the chunk loop's own bookkeeping.
struct RateMeter {
    samples: VecDeque<(Instant, u64)>,
    window: Duration,
}

impl RateMeter {
    fn new(window: Duration) -> Self {
        Self {
            samples: VecDeque::new(),
            window,
        }
    }

    fn record(&mut self, now: Instant, cumulative_bytes: u64) {
        self.samples.push_back((now, cumulative_bytes));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > self.window)
        {
            // Keep one sample older than the window so a slow link, which may
            // only produce a chunk every few seconds, still has a baseline.
            if self.samples.len() <= 2 {
                break;
            }
            self.samples.pop_front();
        }
    }

    /// `None` until the window holds a real interval; a rate computed off a
    /// few milliseconds of one chunk is worse than no rate at all.
    fn bytes_per_sec(&self) -> Option<f64> {
        let (first_at, first_bytes) = *self.samples.front()?;
        let (last_at, last_bytes) = *self.samples.back()?;
        let elapsed = last_at.duration_since(first_at).as_secs_f64();
        if elapsed < 0.5 || last_bytes <= first_bytes {
            return None;
        }
        Some((last_bytes - first_bytes) as f64 / elapsed)
    }
}

fn eta_secs(remaining_bytes: u64, bytes_per_sec: Option<f64>) -> Option<u64> {
    let rate = bytes_per_sec?;
    if rate <= 0.0 {
        return None;
    }
    Some((remaining_bytes as f64 / rate).ceil() as u64)
}

// ---------------------------------------------------------------------------
// Free space
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod free_space {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    // Declared inline instead of enabling another `windows` crate feature:
    // one symbol, a trivial and frozen ABI, and the crate itself is already
    // Windows-gated for DPAPI.
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_bytes_available_to_caller: *mut u64,
            total_number_of_bytes: *mut u64,
            total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }

    pub fn bytes_available(dir: &Path) -> Option<u64> {
        let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut available: u64 = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(available)
    }
}

#[cfg(not(windows))]
mod free_space {
    use std::path::Path;

    /// std has no portable free-space API and the shipped target is Windows
    /// only, so the dev/CI build skips the precheck rather than take on a
    /// libc dependency for it. `None` means "unknown", which
    /// [`super::space_shortfall`] treats as "do not block".
    pub fn bytes_available(_dir: &Path) -> Option<u64> {
        None
    }
}

/// The user-facing complaint when the volume cannot hold the bundle, or
/// `None` when it can (or when free space could not be determined at all —
/// an unknown must never block a download that would have worked).
fn space_shortfall(free_bytes: Option<u64>, needed_bytes: u64) -> Option<String> {
    let free = free_bytes?;
    let required = needed_bytes.saturating_add(FREE_SPACE_MARGIN);
    if free >= required {
        return None;
    }
    Some(format!(
        "not enough free disk space for the model files: {} available, {} needed",
        human_bytes(free),
        human_bytes(required)
    ))
}

fn human_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    format!("{:.1} GB", bytes as f64 / GB)
}

// ---------------------------------------------------------------------------
// Streaming download
// ---------------------------------------------------------------------------

/// Where progress ticks go. `download_models` forwards them to the webview;
/// tests use a sink that drops them, which is what lets the streaming and
/// verification path be exercised without a Tauri `AppHandle`.
type ProgressSink<'a> = &'a (dyn Fn(DownloadProgress) + Sync);

/// Per-file view of the whole-bundle progress bookkeeping. Bundling it keeps
/// the streaming functions down to arguments that are actually about the
/// transfer.
struct ProgressReporter<'a> {
    sink: ProgressSink<'a>,
    key: &'a str,
    files_done: usize,
    files_total: usize,
    bytes_done_before: u64,
    grand_total_bytes: u64,
}

impl ProgressReporter<'_> {
    fn tick(&self, file_bytes_done: u64, file_bytes_total: u64, rate: Option<f64>) {
        let overall_bytes_done = self.bytes_done_before + file_bytes_done;
        let overall_percent = if self.grand_total_bytes > 0 {
            (overall_bytes_done as f64 / self.grand_total_bytes as f64 * 100.0).clamp(0.0, 100.0)
        } else {
            100.0
        };
        let remaining = self.grand_total_bytes.saturating_sub(overall_bytes_done);
        (self.sink)(DownloadProgress {
            current_file: self.key.to_string(),
            file_bytes_done,
            file_bytes_total,
            files_done: self.files_done,
            files_total: self.files_total,
            overall_percent,
            bytes_per_sec: rate,
            eta_secs: eta_secs(remaining, rate),
        });
    }
}

/// Failure modes the bundle loop has to tell apart: an operator stop is not
/// an error and must never be reported as one.
enum DownloadError {
    Cancelled,
    Failed(String),
}

impl From<String> for DownloadError {
    fn from(message: String) -> Self {
        DownloadError::Failed(message)
    }
}

/// Streams an existing `.part` through `hasher` and returns its length, so a
/// resumed transfer produces the same digest as a single-pass one. A missing
/// `.part` is length 0, not an error.
///
/// The prefix is hashed rather than trusted: nothing about a leftover partial
/// is assumed, it just avoids re-fetching bytes that the final digest gate
/// still has to vouch for.
fn hash_existing_prefix(path: &Path, hasher: &mut Sha256) -> std::io::Result<u64> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            return Ok(total);
        }
        hasher.update(&buf[..read]);
        total += read as u64;
    }
}

/// Streams one file to `<dest>.part`, verifies it against the pinned digest,
/// then atomically renames it into place.
///
/// A `.part` from an interrupted attempt is resumed with a `Range` request
/// rather than discarded: the escalation GGUF is 1.8 GB, and on an office
/// link a restart-from-zero policy means it never finishes at all. Resuming
/// cannot weaken integrity because the prefix goes through the same hasher
/// and the whole-file digest below is what decides.
async fn download_one(
    client: &reqwest::Client,
    target: &DownloadTarget,
    emitter: &ProgressReporter<'_>,
    cancel: &AtomicBool,
) -> Result<u64, DownloadError> {
    use futures_util::StreamExt;
    use reqwest::header::{CONTENT_LENGTH, RANGE};
    use reqwest::StatusCode;

    if let Some(parent) = target.dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let part_path = part_path_for(&target.dest);

    let mut hasher = Sha256::new();
    let mut resume_from = hash_existing_prefix(&part_path, &mut hasher)
        .map_err(|e| format!("{}: {e}", part_path.display()))?;

    let send = |from: u64| {
        let mut request = client.get(&target.url);
        if from > 0 {
            request = request.header(RANGE, format!("bytes={from}-"));
        }
        request.send()
    };

    let mut response = send(resume_from)
        .await
        .map_err(|e| format!("GET {} failed: {e}", target.url))?;
    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE && resume_from > 0 {
        // The leftover partial is at least as long as what the server is
        // offering, so it is stale rather than resumable. Start over.
        resume_from = 0;
        hasher = Sha256::new();
        response = send(0)
            .await
            .map_err(|e| format!("GET {} failed: {e}", target.url))?;
    }
    if !response.status().is_success() {
        return Err(format!("GET {} returned HTTP {}", target.url, response.status()).into());
    }
    if resume_from > 0 && response.status() != StatusCode::PARTIAL_CONTENT {
        // The server ignored Range and is sending the whole file: truncate
        // and re-hash from scratch rather than concatenate two prefixes.
        resume_from = 0;
        hasher = Sha256::new();
    }

    // `Content-Length` on a 206 is the length of *this* body, not the file.
    let declared_body_len = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.parse::<u64>().ok());
    let file_bytes_total = declared_body_len
        .map(|len| resume_from + len)
        .unwrap_or(target.size_hint);

    let mut file = if resume_from > 0 {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&part_path)
            .map_err(|e| format!("{}: {e}", part_path.display()))?
    } else {
        std::fs::File::create(&part_path).map_err(|e| format!("{}: {e}", part_path.display()))?
    };

    let mut body_written: u64 = 0;
    let mut last_emit = Instant::now() - PROGRESS_THROTTLE;
    let mut meter = RateMeter::new(RATE_WINDOW);
    meter.record(Instant::now(), resume_from);
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            // Keep the partial: it is exactly what makes "cancel now, finish
            // tonight" work, and the digest gate still guards correctness.
            let _ = file.sync_all();
            return Err(DownloadError::Cancelled);
        }
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => {
                let _ = file.sync_all();
                return Err(format!("download of {} interrupted: {e}", target.url).into());
            }
        };
        file.write_all(&chunk)
            .map_err(|e| format!("{}: {e}", part_path.display()))?;
        hasher.update(&chunk);
        body_written += chunk.len() as u64;

        let now = Instant::now();
        meter.record(now, resume_from + body_written);
        if now.duration_since(last_emit) >= PROGRESS_THROTTLE {
            last_emit = now;
            emitter.tick(
                resume_from + body_written,
                file_bytes_total,
                meter.bytes_per_sec(),
            );
        }
    }
    file.sync_all()
        .map_err(|e| format!("{}: {e}", part_path.display()))?;
    drop(file);

    if let Some(declared) = declared_body_len {
        if body_written != declared {
            // A body that ends short of its own Content-Length is a truncated
            // transfer, not a short file; say so instead of blaming the hash.
            return Err(format!(
                "download of {} ended after {body_written} of {declared} declared bytes",
                target.url
            )
            .into());
        }
    }

    let total = resume_from + body_written;
    let digest = hex::encode(hasher.finalize());
    if digest != target.expected_sha256 {
        let _ = std::fs::remove_file(&part_path);
        return Err(format!(
            "{} failed its integrity check (expected SHA-256 {}, got {digest}); \
             the file was discarded and nothing was installed",
            target.key, target.expected_sha256
        )
        .into());
    }

    // Final tick regardless of the throttle, so the bar always visibly
    // reaches 100% for this file even if the last chunk landed inside the
    // throttle window.
    emitter.tick(total, total, meter.bytes_per_sec());

    std::fs::rename(&part_path, &target.dest)
        .map_err(|e| format!("{}: {e}", target.dest.display()))?;
    Ok(total)
}

/// Ensures one target exists and matches its pinned digest, downloading it if
/// necessary. Returns the number of bytes it contributes to the running
/// overall-progress total.
async fn ensure_file(
    client: &reqwest::Client,
    target: &DownloadTarget,
    emitter: &ProgressReporter<'_>,
    cancel: &AtomicBool,
) -> Result<u64, DownloadError> {
    let existing_len = std::fs::metadata(&target.dest)
        .map(|m| m.len())
        .unwrap_or(0);
    if existing_len > 0 {
        let actual = crate::pipeline::hash_file(&target.dest).map_err(|e| e.to_string())?;
        if actual == target.expected_sha256 {
            // Already present and verified — still tick progress so the bar
            // and file counter advance past it.
            emitter.tick(existing_len, existing_len, None);
            return Ok(existing_len);
        }
        // Never silently overwrite, and never adopt whatever is sitting
        // there: a file at this path that is not the pinned bundle member is
        // something the operator has to look at.
        return Err(format!(
            "{} exists but is not the expected model file (expected SHA-256 {}, found {actual}); \
             move it aside before retrying",
            target.dest.display(),
            target.expected_sha256
        )
        .into());
    }

    download_one(client, target, emitter, cancel).await
}

/// The HTTP client the bundle download uses.
///
/// `https_only` is the load-bearing setting: HF resolve URLs redirect to a
/// CDN, and without it a redirect — from a hostile LAN, a TLS-intercepting
/// proxy, or a compromised CDN entry — can walk a 2.4 GB blob that llama.cpp
/// mmaps and parses down onto plaintext HTTP. Timeouts are per-connect and
/// per-read on purpose: a total-duration timeout across a streamed multi-GB
/// body is a bandwidth floor disguised as a safety net.
fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(5))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("could not build HTTP client: {e}"))
}

fn record_outcome(app: Option<&AppHandle>, done: DownloadDone) {
    if let Some(app) = app {
        let _ = app.emit("model-download-done", &done);
    }
    if let Some(error) = &done.error {
        log::error!("model download failed: {error}");
    } else if done.cancelled {
        log::info!("model download cancelled by the operator");
    } else {
        log::info!("model download completed and verified");
    }
    *LAST_OUTCOME.lock().unwrap_or_else(|e| e.into_inner()) = Some(done);
}

/// Downloads BackLog's full model bundle, skipping anything already present
/// and digest-valid. Progress streams out via the `model-download-progress`
/// event; a terminal `model-download-done` event always follows, and the same
/// outcome is retained for [`model_download_status`] so a user who was not
/// looking at the panel can still find out what happened.
#[tauri::command]
pub async fn download_models(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let models_dir = resolve_models_dir(&app);
    std::fs::create_dir_all(&models_dir)
        .map_err(|e| format!("could not create {}: {e}", models_dir.display()))?;

    let (primary_gguf, escalation_gguf) = {
        let cfg = state.cfg.lock().unwrap();
        (
            cfg.slm_primary_gguf.clone(),
            cfg.slm_escalation_gguf.clone(),
        )
    };
    let targets = download_targets(&models_dir, &primary_gguf, &escalation_gguf);

    CANCEL.store(false, Ordering::SeqCst);
    let emit = |progress: DownloadProgress| {
        let _ = app.emit("model-download-progress", &progress);
    };
    let outcome = run_bundle(&targets, &models_dir, &emit, &CANCEL).await;

    let (ok, cancelled, error) = match &outcome {
        Ok(()) => (true, false, None),
        Err(DownloadError::Cancelled) => (false, true, None),
        Err(DownloadError::Failed(message)) => (false, false, Some(message.clone())),
    };
    record_outcome(
        Some(&app),
        DownloadDone {
            ok,
            cancelled,
            error: error.clone(),
            finished_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    download_command_result(outcome)
}

/// Tauri treats `Err` as a failed command and presents its generic command
/// error path. An operator cancellation is already represented by the
/// structured terminal event/status above, so it completes the command
/// cleanly while preserving real failures as errors.
fn download_command_result(outcome: Result<(), DownloadError>) -> Result<(), String> {
    match outcome {
        Ok(()) => Ok(()),
        Err(DownloadError::Cancelled) => Ok(()),
        Err(DownloadError::Failed(message)) => Err(message),
    }
}

/// The bundle loop, free of Tauri types so it can be driven from tests.
async fn run_bundle(
    targets: &[DownloadTarget],
    models_dir: &Path,
    sink: ProgressSink<'_>,
    cancel: &AtomicBool,
) -> Result<(), DownloadError> {
    let grand_total_bytes: u64 = targets.iter().map(|t| t.size_hint).sum();
    let files_total = targets.len();

    // Anything already on disk is not going to be fetched again, so it must
    // not be counted against free space; without this, a machine that is
    // merely re-verifying an installed bundle could be told it is full.
    let needed_bytes: u64 = targets
        .iter()
        .filter(|t| !t.dest.exists())
        .map(|t| t.size_hint)
        .sum();
    if let Some(problem) = space_shortfall(free_space::bytes_available(models_dir), needed_bytes) {
        return Err(problem.into());
    }

    let client = build_client()?;
    let mut lock = BTreeMap::new();
    let mut bytes_done: u64 = 0;
    for (files_done, target) in targets.iter().enumerate() {
        let emitter = ProgressReporter {
            sink,
            key: target.key,
            files_done,
            files_total,
            bytes_done_before: bytes_done,
            grand_total_bytes,
        };
        bytes_done += ensure_file(&client, target, &emitter, cancel).await?;
        lock.insert(target.key.to_string(), target.expected_sha256.to_string());
    }
    // Descriptive only; see `write_lock`.
    write_lock(&models_dir.join("models.lock.json"), &lock)?;
    Ok(())
}

/// Stop an in-flight bundle download. The partially fetched `.part` files are
/// deliberately kept, so restarting picks up where this left off.
// `allow(dead_code)` only until `lib.rs`'s `generate_handler!` list names this
// command; it is unreferenced from Rust by design, the webview is the caller.
#[allow(dead_code)]
#[tauri::command]
pub fn cancel_model_download() {
    CANCEL.store(true, Ordering::SeqCst);
    log::info!("model download cancellation requested");
}

/// The last terminal outcome of a bundle download, so the Settings panel can
/// re-render a failure the user missed instead of looking identical to
/// "never started".
// See the note on `cancel_model_download`: unreferenced from Rust until the
// command is registered in `lib.rs`.
#[allow(dead_code)]
#[tauri::command]
pub fn model_download_status() -> Option<DownloadDone> {
    LAST_OUTCOME
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    fn noop_sink() -> impl Fn(DownloadProgress) + Sync {
        |_| {}
    }

    /// A one-shot loopback HTTP/1.1 server. Enough to exercise the streaming,
    /// resume and verification paths without a network dependency or a new
    /// dev-dependency; `serve` returns the base URL and the request's `Range`
    /// header (if any) once the exchange has finished.
    fn serve_once(body: Vec<u8>, honor_range: bool) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut range = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("range:") {
                    range = value.trim().to_string();
                }
            }
            let start = if honor_range {
                range
                    .strip_prefix("bytes=")
                    .and_then(|v| v.trim_end_matches('-').parse::<usize>().ok())
                    .unwrap_or(0)
            } else {
                0
            };
            let slice = &body[start.min(body.len())..];
            let head = if start > 0 {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    slice.len(),
                    start,
                    body.len() - 1,
                    body.len()
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                )
            };
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(slice);
            let _ = stream.flush();
            let _ = tx.send(range);
        });
        (format!("http://127.0.0.1:{port}/model.gguf"), rx)
    }

    /// A client with the shipped timeout shape but no `https_only`, so a
    /// loopback test server can stand in for the Hub.
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    fn target_for(dest: PathBuf, url: String, body: &[u8], digest: &'static str) -> DownloadTarget {
        DownloadTarget {
            key: "test.gguf",
            url,
            expected_sha256: digest,
            dest,
            size_hint: body.len() as u64,
        }
    }

    fn emitter_for<'a>(sink: ProgressSink<'a>, key: &'a str, total: u64) -> ProgressReporter<'a> {
        ProgressReporter {
            sink,
            key,
            files_done: 0,
            files_total: 1,
            bytes_done_before: 0,
            grand_total_bytes: total,
        }
    }

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
        assert_eq!(
            download_url("owner/repo", "a file.bin"),
            "https://huggingface.co/owner/repo/resolve/main/a%20file.bin"
        );
    }

    #[test]
    fn every_spec_entry_resolves_to_a_valid_download_url() {
        // Cheap sanity net for future MODEL_FILES edits: every entry must
        // still produce a well-formed https URL under the HF host.
        for file in MODEL_FILES {
            let url = download_url(file.repo, file.hf_path);
            assert!(url.starts_with("https://huggingface.co/"), "{url}");
            assert!(
                url.ends_with(&format!("/resolve/main/{}", file.hf_path)),
                "{url}"
            );
        }
    }

    #[test]
    fn model_files_has_no_duplicate_targets() {
        let mut seen = std::collections::HashSet::new();
        for file in MODEL_FILES {
            assert!(
                seen.insert(file.target),
                "duplicate target: {}",
                file.target
            );
        }
    }

    #[test]
    fn every_spec_entry_pins_a_well_formed_digest() {
        for file in MODEL_FILES {
            assert_eq!(
                file.expected_sha256.len(),
                64,
                "{} is not a SHA-256",
                file.target
            );
            assert!(
                file.expected_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{} must be lowercase hex",
                file.target
            );
        }
    }

    /// The staging script (`models/download_models.py`) and this downloader
    /// must agree bit for bit about what the bundle is, or a machine
    /// provisioned by one would be rejected by the other.
    #[test]
    fn embedded_digests_match_the_committed_models_lock() {
        let lock_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../models/models.lock.json");
        let raw = std::fs::read_to_string(&lock_path)
            .unwrap_or_else(|e| panic!("{}: {e}", lock_path.display()));
        let lock: BTreeMap<String, String> = serde_json::from_str(&raw).unwrap();

        for file in MODEL_FILES {
            assert_eq!(
                lock.get(file.target).map(String::as_str),
                Some(file.expected_sha256),
                "models.lock.json disagrees about {}",
                file.target
            );
        }
        assert_eq!(
            lock.len(),
            MODEL_FILES.len(),
            "models.lock.json lists files this downloader does not know about"
        );
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
        assert_eq!(
            resolve_configured_model_path(models_dir, &configured, PRIMARY_GGUF_NAME),
            configured
        );
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
        let once = resolve_configured_model_path(
            models_dir,
            Path::new("models/x.gguf"),
            PRIMARY_GGUF_NAME,
        );
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
        let escalation_target = targets
            .iter()
            .find(|t| t.key == ESCALATION_GGUF_NAME)
            .unwrap();
        assert_eq!(escalation_target.dest, escalation);
    }

    #[test]
    fn download_targets_separate_colliding_model_destinations() {
        let models_dir = Path::new("/app-data/models");
        let shared = Path::new("/custom/primary.gguf");
        let targets = download_targets(models_dir, shared, shared);

        let primary = targets
            .iter()
            .find(|target| target.key == PRIMARY_GGUF_NAME)
            .unwrap();
        let escalation = targets
            .iter()
            .find(|target| target.key == ESCALATION_GGUF_NAME)
            .unwrap();

        assert_ne!(primary.dest, escalation.dest);
        assert_eq!(primary.dest, models_dir.join("Qwen3-0.6B-Q8_0.gguf"));
        assert_eq!(escalation.dest, models_dir.join("Qwen3-1.7B-Q8_0.gguf"));
    }

    #[test]
    fn download_targets_covers_every_spec_entry_exactly_once() {
        let targets = download_targets(Path::new("/m"), Path::new("/p.gguf"), Path::new("/e.gguf"));
        assert_eq!(targets.len(), MODEL_FILES.len());
        for target in &targets {
            assert!(target.url.starts_with("https://"), "{}", target.url);
        }
    }

    #[test]
    fn part_path_appends_suffix_without_losing_the_original_name() {
        let dest = Path::new("/app-data/models/Qwen3-1.7B-Q8_0.gguf");
        assert_eq!(
            part_path_for(dest),
            Path::new("/app-data/models/Qwen3-1.7B-Q8_0.gguf.part")
        );
    }

    #[test]
    fn lock_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.lock.json");
        let mut lock = BTreeMap::new();
        lock.insert("Qwen3-0.6B-Q8_0.gguf".to_string(), "a".repeat(64));
        write_lock(&path, &lock).unwrap();
        let read_back: BTreeMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read_back, lock);
    }

    /// The whole point of the resume path: a `.part` prefix plus the bytes
    /// that follow must hash exactly like a single-pass download, or the
    /// digest gate would reject every resumed transfer.
    #[test]
    fn hashing_a_part_prefix_plus_the_remainder_equals_a_single_pass_digest() {
        let dir = tempfile::tempdir().unwrap();
        let whole: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let split = 123_457;

        let part = dir.path().join("model.gguf.part");
        std::fs::write(&part, &whole[..split]).unwrap();

        let mut resumed = Sha256::new();
        let prefix_len = hash_existing_prefix(&part, &mut resumed).unwrap();
        assert_eq!(prefix_len, split as u64);
        resumed.update(&whole[split..]);

        let mut single_pass = Sha256::new();
        single_pass.update(&whole);

        assert_eq!(
            hex::encode(resumed.finalize()),
            hex::encode(single_pass.finalize())
        );
    }

    #[test]
    fn hashing_an_absent_part_is_a_zero_length_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut hasher = Sha256::new();
        assert_eq!(
            hash_existing_prefix(&dir.path().join("nope.part"), &mut hasher).unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn download_one_rejects_a_body_whose_digest_differs_and_installs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        let body = b"a substituted GGUF with a heap-overflow payload in it".to_vec();
        let (url, _rx) = serve_once(body.clone(), true);

        // The pinned digest of some *other* content: what a swapped body on
        // the wire looks like from here.
        let expected: &'static str =
            "0000000000000000000000000000000000000000000000000000000000000000";
        let target = target_for(dest.clone(), url, &body, expected);
        let sink = noop_sink();
        let emitter = emitter_for(&sink, target.key, target.size_hint);

        let error = download_one(&test_client(), &target, &emitter, &AtomicBool::new(false))
            .await
            .expect_err("a digest mismatch must fail");
        let DownloadError::Failed(message) = error else {
            panic!("a digest mismatch is a failure, not a cancellation");
        };
        assert!(message.contains("integrity check"), "{message}");
        assert!(
            !dest.exists(),
            "nothing may be installed at the destination"
        );
        assert!(
            !part_path_for(&dest).exists(),
            "a body that failed verification must not be left to be resumed"
        );
    }

    #[tokio::test]
    async fn download_one_verifies_and_installs_a_matching_body() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        let body = b"the real bytes".to_vec();
        let digest: &'static str = Box::leak(hex::encode(Sha256::digest(&body)).into_boxed_str());

        let (url, _rx) = serve_once(body.clone(), true);
        let target = target_for(dest.clone(), url, &body, digest);
        let sink = noop_sink();
        let emitter = emitter_for(&sink, target.key, target.size_hint);

        let written = download_one(&test_client(), &target, &emitter, &AtomicBool::new(false))
            .await
            .map_err(|_| "download failed")
            .unwrap();
        assert_eq!(written, body.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!part_path_for(&dest).exists());
    }

    #[tokio::test]
    async fn download_one_resumes_from_an_existing_part_and_still_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        let body: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
        let split = 17_000usize;
        std::fs::write(part_path_for(&dest), &body[..split]).unwrap();

        let digest: &'static str = Box::leak(hex::encode(Sha256::digest(&body)).into_boxed_str());
        let (url, rx) = serve_once(body.clone(), true);
        let target = target_for(dest.clone(), url, &body, digest);
        let sink = noop_sink();
        let emitter = emitter_for(&sink, target.key, target.size_hint);

        let written = download_one(&test_client(), &target, &emitter, &AtomicBool::new(false))
            .await
            .map_err(|_| "resume failed")
            .unwrap();

        assert_eq!(written, body.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(
            rx.recv().unwrap(),
            format!("bytes={split}-"),
            "the resumed request must ask only for the missing tail"
        );
    }

    /// A server that ignores `Range` (some corporate proxies do) sends the
    /// whole file with a 200. Concatenating that onto the existing prefix
    /// would corrupt the file, so the prefix must be thrown away instead.
    #[tokio::test]
    async fn download_one_starts_over_when_the_server_ignores_range() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        let body: Vec<u8> = (0..9_000u32).map(|i| (i % 199) as u8).collect();
        std::fs::write(part_path_for(&dest), &body[..4_000]).unwrap();

        let digest: &'static str = Box::leak(hex::encode(Sha256::digest(&body)).into_boxed_str());
        let (url, _rx) = serve_once(body.clone(), false);
        let target = target_for(dest.clone(), url, &body, digest);
        let sink = noop_sink();
        let emitter = emitter_for(&sink, target.key, target.size_hint);

        download_one(&test_client(), &target, &emitter, &AtomicBool::new(false))
            .await
            .map_err(|_| "restart failed")
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[tokio::test]
    async fn a_cancelled_download_keeps_its_partial_and_is_not_reported_as_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        let body: Vec<u8> = vec![7u8; 200_000];
        let digest: &'static str = Box::leak(hex::encode(Sha256::digest(&body)).into_boxed_str());
        let (url, _rx) = serve_once(body.clone(), true);
        let target = target_for(dest.clone(), url, &body, digest);
        let sink = noop_sink();
        let emitter = emitter_for(&sink, target.key, target.size_hint);

        // Already cancelled when the first chunk lands.
        let cancel = AtomicBool::new(true);
        let error = download_one(&test_client(), &target, &emitter, &cancel)
            .await
            .expect_err("a cancelled download must not report success");
        assert!(matches!(error, DownloadError::Cancelled));
        assert!(!dest.exists(), "a cancelled download installs nothing");
        assert!(
            part_path_for(&dest).exists(),
            "the partial must survive so the next attempt can resume it"
        );
    }

    #[test]
    fn an_operator_cancel_is_a_clean_command_outcome() {
        assert!(download_command_result(Err(DownloadError::Cancelled)).is_ok());
    }

    /// The old trust-on-first-use path recorded whatever was already sitting
    /// at the destination as ground truth, which on a fresh install (no lock
    /// file) meant a poisoned GGUF became "already present and verified"
    /// forever. Presence now proves nothing; only the pin does.
    #[tokio::test]
    async fn a_file_already_on_disk_is_accepted_only_when_it_matches_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.gguf");
        let body = b"bytes that were already here".to_vec();
        std::fs::write(&dest, &body).unwrap();
        let sink = noop_sink();

        // An unroutable URL: reaching the network at all would fail the test.
        let url = "https://127.0.0.1:9/model.gguf".to_string();
        let unrelated: &'static str = Box::leak("c".repeat(64).into_boxed_str());
        let wrong = target_for(dest.clone(), url.clone(), &body, unrelated);
        let emitter = emitter_for(&sink, wrong.key, wrong.size_hint);
        let error = ensure_file(&test_client(), &wrong, &emitter, &AtomicBool::new(false))
            .await
            .err()
            .map(|e| match e {
                DownloadError::Failed(message) => message,
                DownloadError::Cancelled => unreachable!(),
            })
            .expect("an unpinned file must never be adopted");
        assert!(error.contains("is not the expected model file"), "{error}");
        assert!(dest.exists(), "the operator's file must be left alone");

        let digest: &'static str = Box::leak(hex::encode(Sha256::digest(&body)).into_boxed_str());
        let right = target_for(dest.clone(), url, &body, digest);
        let emitter = emitter_for(&sink, right.key, right.size_hint);
        let bytes = ensure_file(&test_client(), &right, &emitter, &AtomicBool::new(false))
            .await
            .map_err(|_| "a matching file must be accepted without a download")
            .unwrap();
        assert_eq!(bytes, body.len() as u64);
    }

    /// `https_only` is what stops a redirect from walking the transfer down
    /// to plaintext; asserting the behaviour beats grepping for the setting.
    #[tokio::test]
    async fn the_shipping_client_refuses_plaintext_http() {
        let error = build_client()
            .unwrap()
            .get("http://127.0.0.1:9/model.gguf")
            .send()
            .await
            .expect_err("an http:// URL must never be fetched");
        assert!(
            error.is_builder() || error.to_string().contains("URL scheme"),
            "{error}"
        );
    }

    #[test]
    fn free_space_shortfall_demands_headroom_but_never_blocks_on_an_unknown() {
        let needed = 2_500_000_000u64;
        assert!(
            space_shortfall(None, needed).is_none(),
            "unknown must not block"
        );
        assert!(space_shortfall(Some(needed + FREE_SPACE_MARGIN), needed).is_none());
        let complaint = space_shortfall(Some(needed), needed).expect("no headroom left");
        assert!(complaint.contains("free disk space"), "{complaint}");
    }

    #[test]
    fn rate_meter_reports_throughput_and_an_eta_once_the_window_has_an_interval() {
        let start = Instant::now();
        let mut meter = RateMeter::new(RATE_WINDOW);
        meter.record(start, 0);
        assert!(meter.bytes_per_sec().is_none(), "one sample is not a rate");

        meter.record(start + Duration::from_secs(2), 2_000_000);
        let rate = meter
            .bytes_per_sec()
            .expect("two samples two seconds apart");
        assert!((rate - 1_000_000.0).abs() < 1.0, "{rate}");
        assert_eq!(eta_secs(5_000_000, Some(rate)), Some(5));
        assert_eq!(eta_secs(5_000_000, None), None);
    }

    #[test]
    fn rate_meter_forgets_samples_older_than_the_window() {
        let start = Instant::now();
        let mut meter = RateMeter::new(Duration::from_secs(5));
        for second in 0..30u64 {
            meter.record(start + Duration::from_secs(second), second * 1_000_000);
        }
        assert!(
            meter.samples.len() <= 7,
            "the window must stay bounded, got {}",
            meter.samples.len()
        );
    }
}
