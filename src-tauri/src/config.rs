//! BackLog configuration. Loaded from `backlog.config.json` next to the app
//! data dir; every field has a sane default so first launch works with only
//! the folder paths filled in from the UI.

use crate::filter::{max_bundle_chars, BUDGET_CHARS_PER_TOKEN};
use crate::slm::SLM_CTX_PER_SLOT;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by `Config::load` when a `backlog.config.json` existed but failed to
/// parse, so this launch is running on a backup or on defaults instead of the
/// operator's actual settings. Read by `preflight::run_with` to surface a
/// readiness problem — without this, the operator would only ever discover a
/// silently-defaulted config by noticing their settings looked wrong.
static CONFIG_PARSE_FAILURE: AtomicBool = AtomicBool::new(false);

/// Did the most recent `Config::load` fall back because the on-disk config
/// failed to parse? Reset at the top of every `load` call, so this reflects
/// only the latest launch's outcome, not any earlier one in the same process.
pub fn config_parse_failure() -> bool {
    CONFIG_PARSE_FAILURE.load(Ordering::Relaxed)
}

/// Test-only escape hatch. `cargo test` runs every test in a crate in one
/// process, and most `preflight::run_with` tests build a `Config` directly
/// rather than through `load` — nothing resets this flag for them. Without
/// an explicit reset, a config.rs test exercising the parse-failure path on
/// one thread could leave `config_parse_failure()` reading `true` for an
/// unrelated preflight test asserting `configured` on another.
#[cfg(test)]
pub(crate) fn reset_config_parse_failure_for_tests() {
    CONFIG_PARSE_FAILURE.store(false, Ordering::Relaxed);
}

/// Where a validated document is delivered.  The default intentionally keeps
/// every existing config and Power Automate installation byte-compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    #[default]
    PowerAutomate,
    Local,
}

impl OutputMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PowerAutomate => "power_automate",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Delivery contract. Missing in legacy JSON means Power Automate.
    pub output_mode: OutputMode,
    /// OneDrive-synced folder Power Automate Flow 1 moves intake files into.
    pub processing_dir: PathBuf,
    /// OneDrive-synced folder the app writes per-file manifests into
    /// (Flow 2 triggers on `<outbox_dir>/_manifests`).
    pub outbox_dir: PathBuf,
    /// Native destination root. Its `.backlog` child is private transaction
    /// state; delivered documents themselves are placed directly in this root.
    pub local_output_dir: PathBuf,
    /// Local quarantine for flagged files (not synced).
    pub quarantine_dir: PathBuf,
    /// Local cache: converted markdown + evidence bundles, keyed by sha256.
    pub cache_dir: PathBuf,

    /// llama-server settings.
    pub llama_port: u16,
    pub slm_primary_gguf: PathBuf,
    pub slm_escalation_gguf: PathBuf,
    pub slm_parallel: u8,
    pub slm_escalation_parallel: u8,
    /// Requests EITHER llama-server serves before being killed and respawned —
    /// llama.cpp Windows RSS growth is unfixed upstream
    /// (ggml-org/llama.cpp#24356). 0 disables recycling.
    ///
    /// Was 64, from a measurement of 3.45->4.45 GB over 21 files on this same
    /// 0.6B/1.7B pair — but at the old 4096-token slot. Growth tracks the KV
    /// cache the server is churning through, and the slot is now 6656, so that
    /// threshold no longer bounds what it was chosen to bound. Measured on this
    /// workload at ctx 6656, parallel 1:
    ///
    /// | server | at rest | after 3 | after 16 |
    /// |---|---|---|---|
    /// | Qwen3-1.7B-Q8_0 | 2,860 MiB WS | — | 5,785 MiB WS |
    /// | Qwen3-4B-Q4_K_M | 5,068 MiB WS / 2,842 private | 6,186 / 3,880 | 9,258 MiB WS |
    ///
    /// The 1.7B row is the one that binds what ships: it is this build's
    /// escalation server, and it doubles its working set inside 16 requests. At
    /// 64 it would pass 5.8 GB on the 16 GB deployment target long before
    /// recycling, on top of the primary beside it. The 4B row is from the
    /// rejected tier sweep (see `default_primary_gguf_for_ram`) and is kept
    /// because it is the evidence behind `GgufShape::overhead_bytes`, not
    /// because anything ships it. The 0.6B primary was never measured at this
    /// context — which is a reason to keep the threshold low, not a reason to
    /// assume the small model is well behaved.
    ///
    /// The growth is front-loaded, so a low threshold is what actually bounds
    /// it.
    ///
    /// 8 is affordable because respawning is cheap and getting cheaper the
    /// more often you do it: a warm reload measured **3.5-3.6 s** against
    /// naming requests of 35-84 s, so recycling every eighth request costs
    /// about 1.3% of wall clock. The file is still in the OS page cache
    /// immediately after a recycle, which is what keeps the reload warm.
    /// `cache_prompt` is already false, so a recycle discards nothing that was
    /// going to be reused.
    pub slm_recycle_after_requests: u32,
    /// Seconds since the escalation server's last request COMPLETED (never
    /// mid-request — see `SlmLane::reap_idle_escalation`) before it is
    /// dropped. 0 disables idle-reaping (resident for the process lifetime).
    pub slm_escalation_idle_secs: u64,
    /// Max evidence tokens (approximate, chars/4) sent to the SLM on rungs 1
    /// and 2. Rung 3 does not get a second field: it is derived from this one
    /// by `escalation_evidence_token_budget`, so raising this raises both
    /// together and the two can never be configured into an inversion where
    /// the escalation attempt sees *less* than the attempt it is retrying.
    /// `validate` caps it at `max_evidence_token_budget()`, past which the
    /// bundle, the system prompt and the answer stop fitting in one slot.
    pub evidence_token_budget: usize,
    /// Operator-supplied naming preferences appended to the SLM system prompt
    /// as a subordinate "Operator preferences" section (see
    /// `SlmLane::build_system_prompt`). Empty — the default — leaves the
    /// measured core prompt byte-identical. `normalize` unifies line endings
    /// and strips control characters; `validate` caps the trimmed text at
    /// 600 characters rather than truncating it silently.
    pub custom_naming_notes: String,

    /// Optional fine-tuned Ettin token classifier directory (HF format).
    /// Empty string disables the Ettin lane gracefully.
    pub ettin_model_dir: String,

    /// How many `convertd` processes to run, and therefore how many documents
    /// can be converted at once. Each worker is a separate Python process that
    /// converges toward `CONVERTD_WORKER_RSS_MB` resident once it has serviced
    /// any OCR or langid work, so this is a memory knob as well as a
    /// throughput one — see `convert_workers_ram_ceiling`.
    pub convert_workers: usize,
    /// Idle convertd workers beyond this floor are eligible for reaping by
    /// `Sidecar::spawn_idle_reaper`. Default 1: always keep one warm so the
    /// next request after a lull skips the ~1s cold spawn.
    pub convert_min_idle_workers: usize,
    /// Seconds an idle convertd worker may sit before the reaper retires it.
    /// 0 disables reaping (pre-feature behavior: workers only ever grow).
    pub convert_idle_reap_secs: u64,

    /// Maximum wait for one convertd request. A timed-out process is killed
    /// and lazily respawned on the next request. The `ocr` operation gets
    /// three times this, capped at 300 s (see
    /// `sidecar.rs::OCR_TIMEOUT_MULTIPLIER`).
    pub sidecar_timeout_secs: u64,

    /// Pace manifest emission (per minute, 0 = unlimited) to stay under
    /// Power Automate connector throttling on huge batches.
    pub manifest_emit_per_min: u32,

    /// Pages sampled for oversized documents.
    ///
    /// Left at 10/3 through the evidence-budget rise from 1500 to 2500, which
    /// is the obvious thing to reopen and the wrong one. These two knobs decide
    /// which pages are ELIGIBLE; `evidence_token_budget` decides how much of
    /// the eligible text actually reaches the model, and it binds first almost
    /// everywhere:
    ///
    /// * `convertd.py::_truncate_pdf_markdown` only engages at all above
    ///   `head + tail` pages AND 40,000 characters of extracted markdown. A
    ///   20-page document keeps 10/20 + 3/20 = 65% of its text; the evidence
    ///   filter then selects 10,000 characters out of that.
    /// * So a wider window does not add evidence, it adds *candidates* — more
    ///   mid-document text competing for an unchanged character budget,
    ///   against identifying signal (dates, parties, form numbers) that is
    ///   front-loaded in real documents.
    ///
    /// The case it would genuinely help is a date or party that appears only
    /// past page 10 of a long document, which is the harvest-window limit
    /// already recorded in `docs/KNOWN_ISSUES.md` — a real limitation, but one
    /// a budget rise neither causes nor fixes.
    ///
    /// Untested rather than validated: the v0.9.0 corpus has no document that
    /// clears both thresholds, so the E2E batch cannot exercise this path.
    /// Tuning it needs a corpus of genuinely long text-layer PDFs first.
    pub max_head_pages: usize,
    pub max_tail_pages: usize,

    /// Filename policy.
    pub max_filename_len: usize,

    /// Retry policy.
    pub max_stage_attempts: u8,
    /// Wall-clock ceiling for one attempt at one file. `slm.rs`'s naming HTTP
    /// timeout must stay at or above this, or the transport gives up while the
    /// stage still believes it has time.
    ///
    /// 90 s was survivable when a rung sent 1500 evidence tokens at a warm
    /// primary. Two things here moved it. `evidence_token_budget` rose to 2500
    /// and `filter.rs`'s coverage fix is what finally made it bind (bundles
    /// went from 6,526-7,652 to 9,277-10,000 characters), so every request
    /// carries more; and rung 3 may cold-load the escalation server first —
    /// 1749 MiB of weights plus a calculated 728 MiB KV preallocation,
    /// CPU-only, on the target laptop. Against naming requests measured at
    /// 35-84 s on their own, a ceiling below 180 does not fail a broken
    /// document, it fails a slow one.
    pub per_file_wall_clock_secs: u64,

    /// Keep converted markdown in the cache after a file is successfully
    /// emitted. Default false: the raw document text is purged on emit so the
    /// cache never accumulates document bodies (flagged files awaiting review
    /// keep their cache until resolved). Set true only to deliberately build
    /// an Ettin training corpus — an explicit, auditable opt-in.
    pub retain_cache: bool,
    /// Days after which an orphaned cache entry is swept on startup.
    pub cache_ttl_days: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output_mode: OutputMode::PowerAutomate,
            processing_dir: PathBuf::new(),
            outbox_dir: PathBuf::new(),
            local_output_dir: PathBuf::new(),
            quarantine_dir: PathBuf::new(),
            cache_dir: PathBuf::new(),
            llama_port: 8137,
            // Apache-2.0 Qwen3 GGUFs (llama.cpp) replace the Liquid-licensed
            // LFM2.5 pair so the app can be redistributed without a
            // non-standard model license. The primary is the same 0.6B on
            // every machine — promoting the 1.7B into that slot was measured
            // and rejected, see `default_primary_gguf_for_ram` — and it is
            // also the model the installer bundles, so a fresh install names
            // its first document with no network at all. Only the escalation
            // tier is chosen from installed RAM, because that is the one that
            // asks whether a *second* server fits beside the first.
            slm_primary_gguf: default_primary_gguf(),
            slm_escalation_gguf: default_slm_escalation_gguf(),
            slm_parallel: default_slm_parallel(),
            slm_escalation_parallel: default_slm_escalation_parallel(),
            slm_recycle_after_requests: 8,
            // Five minutes, not ten. The 1.7B escalation server is 2977 MiB
            // of the 4815 MiB two-server footprint — 62% of the naming lane,
            // calculated; see `slm_parallel_for_ram` — and only the minority
            // of documents that fail twice on the 0.6B ever wake it.
            // Releasing it after five idle minutes is what keeps the steady
            // state on a batch with sparse escalations at 1838 MiB rather
            // than 4815 MiB; at 600 s it frequently never fired inside a
            // batch at all, which made it a knob in name only. Reaping is
            // timestamped from request *completion*, so a longer-running
            // escalation can never be reaped out from under itself.
            slm_escalation_idle_secs: 300,
            evidence_token_budget: 2500,
            custom_naming_notes: String::new(),
            ettin_model_dir: String::new(),
            convert_workers: default_convert_workers(),
            convert_min_idle_workers: 1,
            convert_idle_reap_secs: 300,
            sidecar_timeout_secs: 45,
            manifest_emit_per_min: 0,
            max_head_pages: 10,
            max_tail_pages: 3,
            max_filename_len: 120,
            max_stage_attempts: 3,
            per_file_wall_clock_secs: 180,
            retain_cache: false,
            cache_ttl_days: 7,
        }
    }
}

fn lexical_norm(p: &Path) -> PathBuf {
    // Lexical normalization only — folders may not exist yet, so canonicalize
    // isn't available. Good enough to catch equal/nested paths.
    p.components().collect()
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("backlog.config.json");
    path.with_file_name(format!(".{name}.bak"))
}

/// Where a main config that failed to parse is preserved. Not timestamped —
/// one generation is enough, and it keeps cleanup simple.
fn invalid_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("backlog.config.json");
    path.with_file_name(format!("{name}.invalid"))
}

/// Stable comparison key for configured roots. Windows path identity is
/// case-insensitive even when the `Path` methods used by a Linux CI runner are
/// not, so the separator/case normalization lives here rather than relying on
/// `PathBuf::starts_with`.
fn path_key(path: &Path) -> String {
    let mut key = lexical_norm(path).to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    key.make_ascii_lowercase();
    while key.len() > 1 && key.ends_with('/') {
        key.pop();
    }
    key
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = path_key(left);
    let right = path_key(right);
    left == right
        || (left.starts_with(&right) && left.as_bytes().get(right.len()) == Some(&b'/'))
        || (right.starts_with(&left) && right.as_bytes().get(left.len()) == Some(&b'/'))
}

/// Return true when an existing component of a configured root is a symbolic
/// link or, on Windows, a reparse point (junctions and mount points included).
/// The watcher must not follow these because a seemingly harmless Processing
/// path could otherwise ingest files from outside the operator's chosen root.
fn contains_reparse_point(path: &Path) -> bool {
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

/// Trim surrounding whitespace and one matched pair of surrounding quotes.
///
/// Windows Explorer's "Copy as path" puts the path on the clipboard *with*
/// double quotes, and a hand-edited `backlog.config.json` picks up stray
/// spaces. Either turns into a literal folder name that can never exist, and
/// the user is then told their folder "does not exist" while looking at a
/// value that reads exactly right. Non-UTF-8 paths are left untouched — there
/// is nothing to trim that we could re-encode safely.
fn normalize_path(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    PathBuf::from(normalize_path_text(text))
}

fn normalize_path_text(text: &str) -> String {
    let trimmed = text.trim();
    let unquoted = ['"', '\'']
        .iter()
        .find_map(|q| {
            trimmed
                .strip_prefix(*q)
                .and_then(|rest| rest.strip_suffix(*q))
                .filter(|_| trimmed.len() >= 2)
        })
        .unwrap_or(trimmed);
    unquoted.trim().to_string()
}

/// Clean operator naming notes: unify `\r\n` and bare `\r` line endings into
/// `\n` first (so a Windows paste keeps its line breaks), then drop every
/// control character except that newline, then trim. Deliberately no
/// truncation — an over-long value must fail `validate` loudly instead of
/// being shortened behind the operator's back.
fn normalize_naming_notes(raw: &str) -> String {
    let unified = raw.replace("\r\n", "\n").replace('\r', "\n");
    unified
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

fn default_convert_workers() -> usize {
    let by_cpu = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).clamp(1, 6))
        .unwrap_or(2);
    by_cpu.min(convert_workers_ram_ceiling(total_ram_gib()))
}

impl Config {
    /// Threads each llama-server child may use.
    ///
    /// llama.cpp defaults `--threads` to every logical core when the flag is
    /// absent — and BackLog runs up to two servers (primary + escalation)
    /// beside `convert_workers` Python processes carrying their own ONNX
    /// thread pools. Letting every party claim every core is how a 12-core
    /// machine ends up an order of magnitude slower end-to-end than the
    /// single-lane SIZING.md baseline: the naming servers and the converters
    /// spend the whole batch preempting each other. Leave the convert
    /// workers their cores; two is the floor a server needs to stay
    /// responsive.
    pub fn slm_threads(&self) -> usize {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        cores
            .saturating_sub(self.convert_workers)
            .clamp(2, cores.max(2))
    }
}

/// RSS per convertd worker once its heaviest lazily-loaded component
/// (RapidOCR) and lingua are both live. Measured 450-530 MB in production;
/// 550 sits above that band deliberately. Replaces the ~195 MB figure this
/// ceiling used before OCR+lingua were measured together — MarkItDown alone
/// is close to the old number, but `convertd.py`'s loaders (`_get`,
/// `convertd.py:95-140`) are memoized per-process with no unload path, and
/// any worker that ever services an `ocr` op or a `langid` op (run on
/// effectively every document) keeps that component loaded for the rest of
/// its life. A long-running pool converges toward the worse number.
const CONVERTD_WORKER_RSS_MB: u64 = 550;

/// How many `convertd` workers installed RAM can hold.
///
/// This became a real constraint the moment `Sidecar` grew a process pool.
/// Before that, `convert_workers` only sized a semaphore and every request
/// funnelled through one child, so the value cost nothing in memory however
/// large it was. Now each worker is its own Python process at
/// `CONVERTD_WORKER_RSS_MB` resident once it has served any real document.
///
/// On an 8 GB machine, two workers at the corrected figure (1.1 GB) leaves
/// well under 150 MB of margin after the OS, the app, and the model servers
/// at `slm_parallel: 1` — no room for real work, exactly the thrash mode this
/// module exists to avoid. One worker leaves real slack, so that tier drops
/// from 2 to 1.
///
/// A CPU-derived value below the ceiling still wins — this only caps.
fn convert_workers_ram_ceiling(gib: Option<u64>) -> usize {
    match gib {
        // 8 GB class: ~1.2 GB left after OS/app/SLM@1. One worker (550 MB)
        // leaves real slack; two (1.1 GB) leaves under 150 MB — no margin.
        // This tier drops from 2 to 1.
        Some(g) if g <= 9 => 1,
        // 16 GB class, and the machine BackLog is actually deployed on. This
        // tier drops from 4 to 3, and it is worth being honest that the
        // at-rest arithmetic alone no longer forces that. The 0.6B/1.7B pair
        // holds a calculated 4815 MiB with both servers up
        // (`slm_parallel_for_ram`), which on a 14.7 GB laptop with ~13.7 GiB
        // usable leaves ~9.0 GiB; Windows takes 2.5-3 GiB and the app and
        // WebView2 ~0.4 GiB, so the workers are dividing ~5.6-6.1 GiB and four
        // of them (2.2 GB) would fit on paper.
        //
        // Two things say three anyway. At-rest is not steady state:
        // llama.cpp's Windows RSS growth is unfixed upstream and the 1.7B
        // measured 2,860 -> 5,785 MiB over 16 requests at this context, so a
        // batch spends most of its life somewhere above 4815 MiB and
        // `slm_recycle_after_requests` bounds that drift rather than removing
        // it — the headroom this ceiling is protecting is the headroom that
        // absorbs it. And the fourth worker buys throughput the pipeline
        // cannot use while `Sidecar` serializes conversions, so it is real
        // memory spent against a maybe.
        //
        // The common case is easier still. ~2.3 GiB of the naming lane is
        // mmapped weights that Windows can evict, and the escalation server is
        // reaped after `slm_escalation_idle_secs`, so most of a batch runs
        // with the 1838 MiB primary alone and roughly 7 GiB free.
        Some(g) if g <= 17 => 3,
        // >16 GB: CPU-derived by_cpu (capped 6) binds first, not RAM.
        Some(_) => 6,
        // Match the smallest tier: don't gamble on the smaller machine's behalf.
        None => 1,
    }
}

/// Total physical RAM in GiB, or `None` if the OS will not say.
#[cfg(windows)]
fn total_ram_gib() -> Option<u64> {
    // GlobalMemoryStatusEx, declared inline rather than pulling in a
    // system-info crate for one number.
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    // SAFETY: `status.length` is set to the struct's own size, which is the
    // documented contract, and the pointer is to a live local.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return None;
    }
    Some(status.total_phys / (1024 * 1024 * 1024))
}

#[cfg(not(windows))]
fn total_ram_gib() -> Option<u64> {
    None
}

/// The primary GGUF on every RAM tier, and the one the installer bundles — so
/// a fresh install names its first document without a download, whatever the
/// machine turns out to be. There is no second primary constant on purpose:
/// see `default_primary_gguf_for_ram` for why every tier gets this one.
const DEFAULT_PRIMARY_GGUF: &str = "models/Qwen3-0.6B-Q8_0.gguf";

/// The GGUF a fresh install names for the escalation tier — on a machine that
/// can hold a second server of this size beside the primary. Smaller machines
/// get a collapsed pair instead; see `default_escalation_gguf_for_ram`.
const DEFAULT_ESCALATION_GGUF: &str = "models/Qwen3-1.7B-Q8_0.gguf";

/// The transformer shape of one GGUF this app ships, downloads, or defaults
/// to, plus its pinned size.
///
/// Memory cost is not a property of the file size alone. llama.cpp
/// preallocates the entire KV cache at startup and that cache scales with the
/// *shape* of the attention stack, which not every catalogued model shares:
/// the 0.6B and 1.7B this build ships both have 28 layers, the 4B has 36.
/// Every estimate in this module reads its shape from here instead of
/// hardcoding 28/8/128, which was silently 1.286x optimistic for a 36-layer
/// model — the direction of error that wedges a machine rather than merely
/// wasting a slot on it.
///
/// That the shipped pair is now uniformly 28 layers makes this table more
/// necessary, not less. A hardcoded 28 would look correct against every
/// default and be wrong for exactly the model an operator reaches for when
/// they decide they want more than the defaults give them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GgufShape {
    /// `file_name()` of the GGUF, which is what `shape_for` matches on.
    /// Operators move these files wherever they like, so the directory says
    /// nothing; the name is the only stable identity we have.
    pub basename: &'static str,
    /// `num_hidden_layers` from the model's `config.json`.
    pub layers: u32,
    /// `num_key_value_heads`. Qwen3 is GQA, so this is far below the
    /// attention-head count, and it is the number the KV cache scales with.
    pub kv_heads: u32,
    /// `head_dim` from the model's `config.json`.
    pub head_dim: u32,
    /// On-disk weight bytes — the pinned download size from
    /// `model_download.rs`, so a shape entry and a catalog entry can never
    /// end up describing different files. llama.cpp mmaps these, so they are
    /// not private commit on day one — but a batch touches every weight on
    /// every token, so the working set converges here and sizing has to treat
    /// them as resident.
    pub weight_bytes: u64,
    /// Everything the server holds that is neither weights nor KV cache:
    /// compute buffers, the logit buffer, the tokenizer, the HTTP stack.
    ///
    /// Per-model because it measured wildly per-model, which a single flat
    /// constant hid. At rest, ctx 6656, parallel 1, this workload:
    ///
    /// | server | working set | weights + KV | remainder |
    /// |---|---|---|---|
    /// | Qwen3-1.7B-Q8_0 | 2,860 MiB | 2,477 MiB | ~383 MiB |
    /// | Qwen3-4B-Q4_K_M | 5,068 MiB | 3,318 MiB | **~1,750 MiB** |
    ///
    /// 4.6x the overhead for 1.25x the hidden size, so this does not follow
    /// from the shape fields above and cannot be derived from them. No flat
    /// allowance can be pessimistic for the 4B without being absurd for the
    /// 1.7B — a 200 MiB constant under-budgeted the 4B by 1,550 MiB while its
    /// comment claimed to be generous. So these are recorded facts about
    /// specific pinned files, each rounded up from its measurement, not a
    /// theory of llama.cpp's allocator.
    ///
    /// This is the at-rest cost only. These servers grow as they serve; see
    /// `Config::slm_recycle_after_requests` for how much and for what bounds
    /// it. Sizing against the converged figure would price in memory that
    /// recycling exists to hand back.
    pub overhead_bytes: u64,
}

/// Every GGUF this app ships or downloads, plus one it no longer defaults to
/// but must still be able to price.
///
/// Shapes verified against each HF repo's `config.json` (`num_hidden_layers`,
/// `num_key_value_heads`, `head_dim`); sizes are the pinned bytes from
/// `model_download.rs`. Adding a model here is what teaches the RAM tiers and
/// `Settings` about it — a GGUF absent from this table is sized as if it were
/// the largest entry, which is safe but pessimistic. Removing one is the
/// dangerous direction, and the 4B entry below says why.
const GGUF_SHAPES: &[GgufShape] = &[
    // The primary on every tier, and the installer's bundled GGUF, so a fresh
    // install names its first document without network.
    GgufShape {
        basename: "Qwen3-0.6B-Q8_0.gguf",
        layers: 28,
        kv_heads: 8,
        head_dim: 128,
        weight_bytes: 639_446_688,
        // Not measured at ctx 6656 — the tier sweep covered the two models
        // that change was proposing, not the one it proposed to replace.
        // 500 MiB is the 1.7B's measured allowance, reused unchanged rather
        // than scaled down for the smaller model. This entry is now what
        // every machine's primary tier is budgeted against, so an unmeasured
        // number here must be the pessimistic one, not the flattering one.
        overhead_bytes: 500 * 1024 * 1024,
    },
    // Escalation tier above 9 GiB; collapsed onto the primary below that.
    GgufShape {
        basename: "Qwen3-1.7B-Q8_0.gguf",
        layers: 28,
        kv_heads: 8,
        head_dim: 128,
        weight_bytes: 1_834_426_016,
        // Measured remainder ~383 MiB; rounded up.
        overhead_bytes: 500 * 1024 * 1024,
    },
    // Catalogued, not shipped. No tier defaults to this model — promoting it
    // measured 2.0x the wall clock at matched coverage and was not merged, see
    // `default_primary_gguf_for_ram` — but it stays in the table, for two
    // reasons that both cost memory safety if it were deleted.
    //
    // `largest_known_shape` folds this table, so the 4B is what
    // `shape_or_largest` charges an *unknown* operator-supplied GGUF. Delete
    // it and that pessimistic fallback silently becomes the 1.7B's 2977 MiB —
    // 2241 MiB cheaper than the largest model we have actually weighed, on the
    // one estimate whose entire job is to over-charge.
    //
    // And an operator who points Settings at a 4B by hand is then priced at
    // its real 5218 MiB rather than mistaken for a 1.7B, which is what lets
    // `naming_lane_budget_mib` tell the machines that can hold it apart from
    // the ones that cannot. A model absent from this table is not treated as
    // large; it is treated as unknown, and `model_is_replaceable` declines to
    // touch what it cannot price.
    //
    // 36 layers, not 28 — this is the whole reason this table exists.
    GgufShape {
        basename: "Qwen3-4B-Q4_K_M.gguf",
        layers: 36,
        kv_heads: 8,
        head_dim: 128,
        weight_bytes: 2_497_280_256,
        // Measured remainder ~1,750 MiB; rounded up. Nearly 4x the 1.7B's,
        // which is the finding that killed the flat constant.
        overhead_bytes: 1_900 * 1024 * 1024,
    },
];

/// F16 KV-cache bytes one token of context occupies for a model of this shape.
///
/// `2 (K and V) * layers * kv_heads * head_dim * 2 (bytes per F16 element)`.
/// 114688 B for the 28-layer 0.6B and 1.7B; 147456 B for the 36-layer 4B.
/// llama.cpp allocates `--ctx-size` of these up front, per server, before it
/// serves a single request — which is why this number, not throughput, is
/// what the tiers below are actually deciding.
pub fn kv_bytes_per_token(layers: u32, kv_heads: u32, head_dim: u32) -> u64 {
    2 * layers as u64 * kv_heads as u64 * head_dim as u64 * 2
}

/// The catalogued shape of `gguf`, matched on its file name (case-insensitively,
/// because Windows), or `None` when nothing in `GGUF_SHAPES` matches.
///
/// `None` must never be read as "small". An operator is free to point Settings
/// at some other GGUF, and this module cannot know its layer count — so every
/// memory estimate goes through `shape_or_largest`, which substitutes the
/// largest shape in the catalog. An over-estimate costs an unused slot; an
/// under-estimate hands the operator a config that thrashes the machine it was
/// meant to protect.
pub fn shape_for(gguf: &Path) -> Option<&'static GgufShape> {
    let name = gguf.file_name()?.to_str()?;
    GGUF_SHAPES
        .iter()
        .find(|shape| shape.basename.eq_ignore_ascii_case(name))
}

/// The largest shape in the catalog, by the only measure that matters here:
/// what one server holding it costs resident. Folded from the table rather
/// than named, so adding a bigger model to `GGUF_SHAPES` cannot leave this
/// pointing at the previous maximum.
fn largest_known_shape() -> &'static GgufShape {
    GGUF_SHAPES
        .iter()
        .max_by_key(|shape| resident_bytes(shape, 1))
        .expect("GGUF_SHAPES is never empty")
}

/// `shape_for`, degraded to `largest_known_shape` for an unknown GGUF. This is
/// the entry point every memory estimate uses; see `shape_for` for why the
/// fallback goes up rather than down.
pub fn shape_or_largest(gguf: &Path) -> &'static GgufShape {
    shape_for(gguf).unwrap_or_else(largest_known_shape)
}

/// Bytes one llama-server holding `shape` with `parallel` slots keeps resident
/// once loaded and idle: mmapped weights + the KV cache llama.cpp preallocates
/// for `SLM_CTX_PER_SLOT * parallel` tokens + the model's own
/// `overhead_bytes`.
///
/// Calculated, but each term is anchored to a measurement: the KV term to the
/// shape fields read from the model's `config.json`, the overhead term to a
/// working-set capture of that exact GGUF at this exact context size. It
/// reproduces the sizing table cited by `slm_parallel_for_ram` exactly, and
/// `the_naming_lane_footprint_matches_the_sizing_table` asserts that so the
/// prose and the arithmetic cannot drift apart.
///
/// **At-rest, not peak.** A server that has served requests is larger — see
/// `Config::slm_recycle_after_requests`, which is the knob that bounds the
/// difference and the reason it is safe to size against the smaller number.
pub fn resident_bytes(shape: &GgufShape, parallel: u8) -> u64 {
    let kv = kv_bytes_per_token(shape.layers, shape.kv_heads, shape.head_dim)
        * SLM_CTX_PER_SLOT as u64
        * parallel as u64;
    shape.weight_bytes + kv + shape.overhead_bytes
}

/// `resident_bytes` rounded to the nearest MiB — the unit the sizing tables
/// and the clamp log messages are written in.
pub fn resident_mib(shape: &GgufShape, parallel: u8) -> u64 {
    (resident_bytes(shape, parallel) + (1 << 19)) >> 20
}

/// How many naming slots to give llama-server, chosen from installed RAM.
///
/// This is a memory knob wearing a concurrency knob's name. `slm.rs` derives
/// `--ctx-size` as `SLM_CTX_PER_SLOT * parallel`, and llama.cpp preallocates
/// the whole KV cache at startup, so the cost is linear, large, and **not the
/// same for every model this module can be pointed at** — the shape table
/// above is what keeps that honest. At `SLM_CTX_PER_SLOT` = 6656, per
/// `kv_bytes_per_token`:
///
/// * 28-layer (0.6B, 1.7B): 6656 * 114688 B = **728 MiB per slot**
/// * 36-layer (4B):         6656 * 147456 B = **936 MiB per slot**
///
/// The escalation tier is a second server, not a second slot: `SlmLane` keeps
/// `primary` and `escalation` separate and the escalation server remains
/// resident until `slm_escalation_idle_secs` reaps it, so a run with active
/// escalations holds both at once. Naming-lane footprints at rest
/// (weights + KV + the model's own `overhead_bytes`):
///
/// | Installed RAM | primary | escalation | both resident |
/// |---|---|---|---|
/// | <= 9 GiB  | 0.6B @1: 610 + 728 + 500 = 1838 MiB | collapsed | 1838 MiB |
/// | <= 17 GiB | 0.6B @1: 610 + 728 + 500 = 1838 MiB | 1.7B @1: 1749 + 728 + 500 = 2977 MiB | 4815 MiB |
/// | > 17 GiB  | 0.6B @2: 610 + 2*728 + 500 = 2566 MiB | 1.7B @1: 2977 MiB | 5543 MiB |
///
/// The primary column does not vary by tier because the model does not: see
/// `default_primary_gguf_for_ram`. Only the slot count and the escalation
/// server move.
///
/// **Calculated, not measured** — every cell is `resident_mib(shape, parallel)`
/// and nothing else, and `the_naming_lane_footprint_matches_the_sizing_table`
/// asserts every footprint in this table, plus the 728 MiB slot term the rows
/// are broken into, so the prose cannot drift from the arithmetic that
/// produced it. What is measured are the *terms*: the KV term comes from shape
/// fields read out of each model's `config.json`, and the overhead term from a
/// working-set capture of that exact GGUF at this exact context size — the
/// 1.7B captured 2,860 MiB at rest against the 2977 MiB calculated here.
///
/// These are recomputed for this pair at this slot size, not inherited. The
/// figures this function used to cite — 590 MB private commit at 1 slot,
/// 1,040 MB at 2, 1,938 MB at 4, 6,078 MB for both servers at 4 — were taken
/// at the old 4096-token slot, where one slot cost 448 MiB instead of 728.
/// Nothing sized before `SLM_CTX_PER_SLOT` moved describes what runs now, even
/// for the models it was measured on.
///
/// The 16 GB class is the machine BackLog is deployed on (~14.7 GB, floored to
/// 14), and it is the tier this whole sizing exists to be correct for. Its
/// 4815 MiB overstates the pressure twice over:
///
/// * **~2.3 GiB of it is mmapped weights**, which are file-backed and
///   evictable. Windows reclaims those under pressure at a cost in speed, not
///   correctness.
/// * **It is the transient, not the steady state.** The escalation server only
///   wakes for a document that failed two attempts and is reaped after
///   `slm_escalation_idle_secs`. Most of a batch runs primary-only, at
///   1838 MiB.
///
/// So memory is not what holds the deployment target at one slot any more —
/// 5543 MiB would fit there. What holds it is that nothing measured says a
/// second slot pays: `Sidecar` serializes every conversion through its worker
/// pool, so cross-file naming overlap is rarely the bottleneck, and the
/// throughput this build is documented at (20.03 s/file, 5.6 h per 1,000) was
/// measured at `slm_parallel: 1`. Shipping a default nobody has run, to buy
/// overlap nobody has demonstrated, is how a sizing table stops being evidence
/// and starts being decoration. The workstation tier gets a second slot
/// because a machine with that much spare RAM has the spare cores to use it;
/// anything past 2 would be the same guess one step further out.
fn default_slm_parallel() -> u8 {
    slm_parallel_for_ram(total_ram_gib())
}

/// The decision itself, split from reading the machine so it can be tested.
///
/// The small branches are the entire reason this function exists and they are
/// the ones a large build machine can never exercise, so they are covered by
/// `slm_parallel_is_chosen_from_installed_ram` rather than by hoping.
fn slm_parallel_for_ram(gib: Option<u64>) -> u8 {
    match gib {
        // 8 GB class: one 1838 MiB server (collapsed — this tier gets no
        // second model) is already most of what is free once Windows and
        // convertd are up. It also runs `convert_workers: 1`, so there is
        // never a second document in flight for a second slot to name.
        Some(g) if g <= 9 => 1,
        // 16 GB class — the deployment target. Room for a second slot
        // (5543 MiB), no evidence it pays; see `default_slm_parallel`.
        Some(g) if g <= 17 => 1,
        // Workstations: a second slot takes the primary to 2566 MiB and the
        // pair to 5543 MiB, which is affordable here and has cores behind it.
        Some(_) => 2,
        // Unknown RAM gets the smallest machine's answer. Guessing large on
        // behalf of a machine that will not say is how you thrash it.
        None => 1,
    }
}

fn default_slm_escalation_parallel() -> u8 {
    slm_escalation_parallel_for_ram(total_ram_gib())
}

/// One slot on every machine, and the argument is deliberately ignored.
///
/// Escalation is the rare third attempt at a document that failed twice, and
/// `SlmLane` serializes it behind the same wall clock as everything else, so a
/// second slot buys overlap that never happens. The price of that nothing fell
/// with the tier decision — **728 MiB**, the 28-layer slot, rather than the
/// 936 MiB a 36-layer escalation model would have cost — but a cheaper nothing
/// is still nothing: it would take the 16 GB class from a calculated 4815 MiB
/// to 5543 MiB and hand back no concurrency it can use. When the tier is
/// collapsed (`slm_escalation_gguf == slm_primary_gguf`) there is no second
/// server for this to size at all.
fn slm_escalation_parallel_for_ram(_gib: Option<u64>) -> u8 {
    1
}

/// Which GGUF a fresh install names for the primary tier.
///
/// The same 0.6B on every machine, today. The tier hook stays because the
/// question it asks — can this machine afford a better primary? — is a real
/// one whose answer is empirical rather than structural, and because the
/// escalation tier below derives its collapsed case from this function, so
/// deleting the parameter would decide two things at once.
///
/// It has been answered once, against the pair that would have replaced this
/// one. Same corpus, same code, same ctx and evidence budget, `slm_parallel: 1`,
/// `convert_workers: 3`, semantic lane confirmed live in every arm:
///
/// | config | s/file | h/1000 | FAITHFUL | NAMES PARTY | TOP ENTITY |
/// |---|---|---|---|---|---|
/// | 0.6B/1.7B, top_k 12 | 24.69 | 6.9 | 24/25 | 25/26 | 19/26 |
/// | 0.6B/1.7B, top_k 17 | **20.03** | **5.6** | 23/25 | 25/26 | 19/26 |
/// | 1.7B/4B, top_k 12 | 38.47 | 10.7 | 24/24 | 24/26 | 16/26 |
/// | 1.7B/4B, top_k 17 | 40.11 | 11.1 | 25/25 | 25/26 | 13/26 |
///
/// At matched evidence coverage (top_k 17) a 1.7B primary costs **2.0x the
/// wall clock** — 40.11 s/file against 20.03 — to buy a two-document
/// faithfulness difference at n=26, which is inside this project's own
/// documented run-to-run variance, while giving back six documents of TOP
/// ENTITY. Rejected on quality-per-second, so this function returns the 0.6B
/// for a 64 GiB workstation for exactly the same reason it returns it for an
/// 8 GB laptop: not affordability, evidence.
///
/// Memory was never what was wrong with it, and saying so is the point of
/// writing this down. A 1.7B primary fits the 16 GB class comfortably: a
/// calculated 2977 MiB against the 0.6B's 1838. If someone re-proposes the
/// promotion on new evidence, the tier that still needs an argument is the
/// small one — the 1.7B is a calculated 1139 MiB more resident, and on an
/// 8 GB machine (7.4 GiB usable, less Windows at 2.5-3 GiB, one convertd
/// worker at 550 MB and the app at ~400 MB) that is most of the margin the
/// tier has. Measure it there before shipping it there, and measure the wall
/// clock at matched coverage rather than at matched `top_k`, because that is
/// the comparison that reversed the last conclusion.
///
/// Unknown RAM would match the smallest tier, as it does everywhere else in
/// this module — that fact is invisible while there is one answer, and it is
/// what `models_are_chosen_from_installed_ram` pins so it stays true if a
/// second answer ever comes back.
fn default_primary_gguf_for_ram(_gib: Option<u64>) -> &'static str {
    DEFAULT_PRIMARY_GGUF
}

fn default_primary_gguf() -> PathBuf {
    PathBuf::from(default_primary_gguf_for_ram(total_ram_gib()))
}

/// Does this machine get a genuinely separate escalation model, or a collapsed
/// pair?
///
/// The 1.7B calculates to 2977 MiB resident, so it is a second server only
/// where one fits beside the primary's 1838 MiB — a 4815 MiB naming lane,
/// which an 8 GB machine (7.4 GiB usable) does not have once Windows, the app
/// and a convertd worker are up. Below the line, `slm_escalation_gguf` is set
/// equal to `slm_primary_gguf` and `SlmLane::escalation_collapsed()` takes
/// over: rung 3 still runs, on a wider evidence bundle
/// (`escalation_evidence_token_budget`), against the server that is already
/// up. So the small tier loses a second opinion, not the third attempt.
///
/// Unknown RAM is treated as the small machine for the same reason
/// `slm_parallel_for_ram` does — a machine that will not report its memory is
/// not an invitation to assume it has plenty.
fn separate_escalation_model_fits(gib: Option<u64>) -> bool {
    matches!(gib, Some(g) if g > 9)
}

/// Which GGUF a fresh install names for the escalation tier. Pure in `gib` so
/// the tiers are testable on a build machine that is none of them.
fn default_escalation_gguf_for_ram(gib: Option<u64>) -> &'static str {
    if separate_escalation_model_fits(gib) {
        DEFAULT_ESCALATION_GGUF
    } else {
        // The *same* string as the primary tier's, not merely a similar one:
        // `SlmLane` detects a collapsed pair by comparing the two paths, so
        // deriving this from `default_primary_gguf_for_ram` is what makes the
        // collapse true by construction instead of by coincidence.
        default_primary_gguf_for_ram(gib)
    }
}

fn default_slm_escalation_gguf() -> PathBuf {
    PathBuf::from(default_escalation_gguf_for_ram(total_ram_gib()))
}

/// At-rest MiB the configured naming lane holds: both servers, or **one** when
/// the pair is collapsed.
///
/// The collapse check is path equality, the same test
/// `SlmLane::escalation_collapsed()` makes, because a collapsed pair is not two
/// servers costing the same thing twice — it is one server that rung 3 reuses.
/// Adding it twice would price the small tiers at double what they hold and
/// clamp configurations that are perfectly safe.
///
/// Sizing goes through `shape_or_largest`, so an uncatalogued model is charged
/// the largest shape in the table. That is the safe direction for the *total*;
/// what it must never become is grounds for replacing the unknown model
/// itself, which is `model_is_replaceable`'s job.
fn naming_lane_mib(
    primary: &Path,
    escalation: &Path,
    primary_parallel: u8,
    escalation_parallel: u8,
) -> u64 {
    let primary_mib = resident_mib(shape_or_largest(primary), primary_parallel);
    if escalation == primary {
        primary_mib
    } else {
        primary_mib + resident_mib(shape_or_largest(escalation), escalation_parallel)
    }
}

/// May this module overwrite `configured` with something else?
///
/// Only when the catalog can price it. Sizing an unknown model pessimistically
/// costs an unused slot; *replacing* it costs the operator the model they
/// explicitly chose, which is a much heavier act than lowering a number and one
/// this module cannot justify on a shape it does not know. An uncatalogued GGUF
/// still contributes its pessimistic `shape_or_largest` figure to
/// `naming_lane_mib`, so it can push the *other* half of the pair down — it
/// simply cannot be sacrificed itself. "We cannot price this" means hands off,
/// never "it fits".
///
/// The configs this exists to repair are the ones an older BackLog wrote naming
/// BackLog's own models, and those are all catalogued by construction.
fn model_is_replaceable(configured: &Path) -> bool {
    shape_for(configured).is_some()
}

/// The at-rest MiB this machine's tier will let the naming lane hold.
///
/// This is the number the model clamp compares against, and getting it from the
/// machine rather than from the tier's default models is the whole point.
/// "Bigger than what a fresh install would have written" is not a reason to
/// override an operator — plenty of machines can hold more than they ship with.
/// "Bigger than the memory actually available" is. The two questions used to be
/// conflated, and the symptom was a 64 GiB workstation having a hand-configured
/// 4B escalation server (a calculated 7056 MiB beside the 0.6B primary, on a
/// box with over 20 GiB free) replaced by the 1.7B, for no reason but that the
/// tier default said 1.7B.
///
/// Derived per tier from the same representative machine every other comment in
/// this module reasons about, with Windows taken at the pessimistic end of its
/// 2.5-3 GiB range and `convert_workers` at that tier's own ceiling:
///
/// | tier | machine | usable | Windows | app | convertd | headroom | budget |
/// |---|---|---|---|---|---|---|---|
/// | <= 9 GiB  | 8 GB   | 7578 MiB  | 3072 | 400 | 1 x 550 | 3556 MiB  | **2300** |
/// | <= 17 GiB | 16 GB  | 14028 MiB | 3072 | 400 | 3 x 550 | 8906 MiB  | **5900** |
/// | > 17 GiB  | 32 GiB | 31744 MiB | 3072 | 400 | 6 x 550 | 24972 MiB | **16600** |
///
/// The budget is headroom / 1.5, rounded **down** to the nearest 100 MiB,
/// because the figures being compared to it are at-rest and a llama-server that
/// has served requests is bigger. 1.5 is not a guess: llama.cpp's Windows RSS
/// growth was measured at this context on both models, and interpolating those
/// captures to the shipped `slm_recycle_after_requests` of 8 gives 1.51x for
/// the 1.7B (2,860 -> ~4,320 of its measured 5,785 at 16) and 1.45x for the 4B
/// (5,068 -> ~7,370 of its measured 9,258 at 16). So the lane may commit two
/// thirds of its headroom at rest and still fit when it has drifted.
///
/// The thinnest case this leaves is the *bottom* of the 16 GB tier — a 10 GiB
/// machine gets the 16 GB budget, because `separate_escalation_model_fits`
/// draws its line at 9 GiB. That boundary predates this budget and is not
/// re-litigated here; it is noted so the next person to move either one knows
/// they are coupled.
fn naming_lane_budget_mib(gib: Option<u64>) -> u64 {
    match gib {
        Some(g) if g <= 9 => 2300,
        Some(g) if g <= 17 => 5900,
        Some(_) => 16600,
        // Unknown RAM gets the smallest machine's budget, as it gets the
        // smallest machine's answer everywhere else in this module.
        None => 2300,
    }
}

/// The largest `evidence_token_budget` that still fits in one llama-server
/// slot beside the prompt and the answer — 4347 at today's constants.
///
/// Composed from other modules' constants, never written down here.
/// [`max_bundle_chars`] is
/// `(SLM_CTX_PER_SLOT - SLM_PROMPT_RESERVE_TOKENS - SLM_MAX_OUTPUT_TOKENS) *
/// CONSERVATIVE_CHARS_PER_TOKEN`, i.e. `(6656 - 640 - 220) * 3` = 17388
/// characters; [`BUDGET_CHARS_PER_TOKEN`] converts that into the optimistic
/// chars/4 unit `evidence_token_budget` is expressed in, so 17388 / 4 = 4347.
///
/// Every one of those numbers is imported rather than restated. That is the
/// whole point: a second copy of 6656/640/220/3/4 living here would let a
/// future retune of the slot size move one home and not the other, and the
/// symptom would be `validate` accepting a budget that `filter.rs` then
/// silently truncates. Truncation is the failure worth engineering against
/// precisely because it is silent — llama.cpp does not refuse an over-length
/// prompt, it drops what does not fit, and the model names the document from
/// evidence with a hole in it.
///
/// `validate` rejects anything above this and
/// `escalation_evidence_token_budget` clamps to it, so no configuration —
/// hand-edited, imported, or migrated forward — can reach that ceiling at all.
pub fn max_evidence_token_budget() -> usize {
    max_bundle_chars() / BUDGET_CHARS_PER_TOKEN
}

impl Config {
    pub fn load(path: &Path) -> Self {
        let parse = |candidate: &Path| -> Option<Self> {
            // A read failure (permission denial, sharing violation, UTF-16
            // content) is not a parse failure — telling the operator their
            // file "failed to parse" when it could not be read sends them
            // debugging the wrong thing.
            let contents = match std::fs::read_to_string(candidate) {
                Ok(text) => text,
                Err(error) => {
                    if candidate.exists() {
                        log::warn!("config read failed for {} ({error})", candidate.display());
                    }
                    return None;
                }
            };
            // Windows editors and PowerShell 5.1's `Set-Content -Encoding utf8`
            // default write a leading UTF-8 BOM that serde_json rejects
            // outright; a BOM alone must never be treated as a parse failure.
            let contents = contents.strip_prefix('\u{FEFF}').unwrap_or(&contents);
            match serde_json::from_str(contents) {
                Ok(cfg) => Some(cfg),
                Err(error) => {
                    log::warn!("config parse failed for {} ({error})", candidate.display());
                    None
                }
            }
        };
        let backup = backup_path(path);
        let main_exists = path.exists();
        let main_parse = parse(path);
        let main_parse_failed = main_exists && main_parse.is_none();
        // Set regardless of what happens to the `.invalid` copy below — the
        // operator's settings failed to parse either way, and preflight must
        // surface that even if the preservation copy itself also failed.
        CONFIG_PARSE_FAILURE.store(main_parse_failed, Ordering::Relaxed);
        let (mut cfg, recovered_from_backup) = match main_parse {
            Some(cfg) => (cfg, false),
            None => match parse(&backup) {
                Some(cfg) => {
                    if !path.exists() {
                        log::warn!(
                            "config file is missing; recovering the last complete backup from {}",
                            backup.display()
                        );
                    }
                    (cfg, true)
                }
                None => (Self::default(), false),
            },
        };
        if main_parse_failed {
            // The file that failed to parse is about to be superseded (by the
            // backup or by defaults) and a later `save` would overwrite it at
            // `path` — copy it aside first so the operator's original bytes
            // are never silently lost.
            let invalid = invalid_path(path);
            // The whole point of the copy is that the original is about to
            // be replaceable; a silent copy failure would leave the operator
            // with no signal that preservation did not happen — so the log
            // reports what the copy actually did, not what it was meant to.
            match std::fs::copy(path, &invalid) {
                Ok(_) => log::error!(
                    "config file {} failed to parse; original preserved at {}",
                    path.display(),
                    invalid.display()
                ),
                Err(error) => log::error!(
                    "config file {} failed to parse and could NOT be preserved at {} ({error}); do not overwrite it",
                    path.display(),
                    invalid.display()
                ),
            }
        }
        if recovered_from_backup {
            let _ = std::fs::rename(&backup, path);
        }
        cfg.normalize();
        cfg.clamp_resources_to_machine();
        cfg
    }

    /// Apply the machine's memory ceilings to loaded and newly submitted
    /// settings. This is intentionally one-directional: conservative custom
    /// values survive, while an old or imported high-memory preset is made
    /// safe before it can start worker processes.
    pub fn clamp_resources_to_machine(&mut self) {
        self.clamp_resources_for_ram(total_ram_gib());
    }

    /// The ceilings themselves, split from reading the machine so that every
    /// tier is testable on a build machine that can only ever be one of them.
    ///
    /// The `default_*` functions only decide what a *fresh* install writes,
    /// and `backlog.config.json` is persistent — so an 8 GB laptop upgrading
    /// from a build whose default was a flat `slm_parallel: 4`, or carrying a
    /// config imported from a larger machine that names the 1.7B as its
    /// primary, would keep that forever and thrash through its whole backfill
    /// having never chosen it. This is the upgrade path for that machine.
    ///
    /// Every clamp here is one-directional. A value at or below what the tier
    /// budgets is left exactly as configured, because someone who lowered it
    /// knows something about their machine that this does not; only an
    /// overcommitment is corrected. And never silently: each clamp says which
    /// knob it moved, what it cost, and what it moved it to, because the only
    /// thing worse than a config being overridden is a config being overridden
    /// invisibly.
    ///
    /// The order matters and is not arbitrary. Slot counts are clamped first,
    /// because the model budget below prices the lane at `slm_parallel` /
    /// `slm_escalation_parallel` and must see the values this run will actually
    /// use — reading them first would charge the pair for slots that are about
    /// to be taken away and clamp models that fit. Convert workers sit in
    /// between because their ceiling is a fixed per-tier count that nothing
    /// else here reads.
    fn clamp_resources_for_ram(&mut self, gib: Option<u64>) {
        let slm_ceiling = slm_parallel_for_ram(gib);
        if self.slm_parallel > slm_ceiling {
            log::warn!(
                "slm_parallel {} exceeds what {} GiB of RAM supports; using {} for this run \
                 (set it explicitly lower to silence this, or see docs/SIZING.md)",
                self.slm_parallel,
                gib.map(|g| g.to_string())
                    .unwrap_or_else(|| "an unknown amount of".into()),
                slm_ceiling
            );
            self.slm_parallel = slm_ceiling;
        }
        let slm_escalation_ceiling = slm_escalation_parallel_for_ram(gib);
        if self.slm_escalation_parallel > slm_escalation_ceiling {
            log::warn!(
                "slm_escalation_parallel {} exceeds what {} GiB of RAM supports; using {} for this run \
                 (set it explicitly lower to silence this, or see docs/SIZING.md)",
                self.slm_escalation_parallel,
                gib.map(|g| g.to_string())
                    .unwrap_or_else(|| "an unknown amount of".into()),
                slm_escalation_ceiling
            );
            self.slm_escalation_parallel = slm_escalation_ceiling;
        }
        let convert_ceiling = convert_workers_ram_ceiling(gib);
        if self.convert_workers > convert_ceiling {
            log::warn!(
                "convert_workers {} exceeds this machine's safe memory budget \
                 (~{CONVERTD_WORKER_RSS_MB} MB/worker); using {}",
                self.convert_workers,
                convert_ceiling
            );
            self.convert_workers = convert_ceiling;
        }
        self.convert_workers = self.convert_workers.max(1);
        // `min_idle_workers >= max_workers` makes `spawn_idle_reaper` inert
        // (see `sidecar.rs`), so there is nothing to clamp here beyond the
        // floor `validate` already enforces on both fields — the reaper
        // simply does not run on a machine whose convert_workers ceiling
        // dropped to 1, e.g. the corrected 8 GB tier.

        // The two model knobs, judged as a pair against an explicit memory
        // budget (`naming_lane_budget_mib`) rather than against the tier's
        // default models. A configured pair that fits is never touched, however
        // far it is from what a fresh install would have written — "not the
        // default" is not a memory problem, and the operator who typed it knows
        // something about their machine that this function does not.
        //
        // Only over-budget pairs come down, and they come down in the order
        // that costs the operator least.
        let budget = naming_lane_budget_mib(gib);
        let machine = || {
            gib.map(|g| g.to_string())
                .unwrap_or_else(|| "an unknown amount of".into())
        };
        let configured = naming_lane_mib(
            &self.slm_primary_gguf,
            &self.slm_escalation_gguf,
            self.slm_parallel,
            self.slm_escalation_parallel,
        );
        if configured > budget {
            // Step 1: give up the second *server* before giving up either
            // model. Escalation degrades gracefully — `escalation_collapsed()`
            // still runs rung 3, on a wider evidence bundle, against the server
            // already up — while the primary is on every document's path. This
            // is a collapse, so it must land on the exact primary path.
            if self.slm_escalation_gguf != self.slm_primary_gguf
                && model_is_replaceable(&self.slm_escalation_gguf)
            {
                log::warn!(
                    "the configured naming pair calculates to ~{} MiB resident, more than the \
                     ~{} MiB {} GiB of RAM budgets for the naming lane; collapsing escalation \
                     onto {} for this run — the third naming attempt still runs, on a wider \
                     evidence bundle, against the server that is already up (name a smaller \
                     pair explicitly to silence this, or see docs/SIZING.md)",
                    configured,
                    budget,
                    machine(),
                    self.slm_primary_gguf.display()
                );
                self.slm_escalation_gguf = self.slm_primary_gguf.clone();
            }
            // Step 2: one server of the configured primary is still too much,
            // so the model itself has to go. Falling back to the tier's own
            // defaults is guaranteed to fit — `every_tier_can_afford_its_own_defaults`
            // is what keeps that guarantee from rotting — and the escalation
            // tier is re-derived from the primary we land on so that a small
            // machine's collapse stays exact.
            let collapsed = naming_lane_mib(
                &self.slm_primary_gguf,
                &self.slm_escalation_gguf,
                self.slm_parallel,
                self.slm_escalation_parallel,
            );
            if collapsed > budget && model_is_replaceable(&self.slm_primary_gguf) {
                let primary = PathBuf::from(default_primary_gguf_for_ram(gib));
                let escalation = PathBuf::from(default_escalation_gguf_for_ram(gib));
                log::warn!(
                    "slm_primary_gguf {} calculates to ~{} MiB resident on its own, more than \
                     the ~{} MiB {} GiB of RAM budgets for the naming lane; using {} for this \
                     run (name a smaller model explicitly to silence this, or see \
                     docs/SIZING.md)",
                    self.slm_primary_gguf.display(),
                    collapsed,
                    budget,
                    machine(),
                    primary.display()
                );
                self.slm_primary_gguf = primary;
                self.slm_escalation_gguf = escalation;
            }
        }
    }

    /// Clean every operator-supplied value in place. Called on load and again
    /// in `set_config`, so a quoted or space-padded path is tolerated whether
    /// it arrived from the Browse dialog, a paste into the text field, or a
    /// hand-edited `backlog.config.json`.
    pub fn normalize(&mut self) {
        for dir in [
            &mut self.processing_dir,
            &mut self.outbox_dir,
            &mut self.local_output_dir,
            &mut self.quarantine_dir,
            &mut self.cache_dir,
            &mut self.slm_primary_gguf,
            &mut self.slm_escalation_gguf,
        ] {
            *dir = normalize_path(dir);
        }
        self.ettin_model_dir = normalize_path_text(&self.ettin_model_dir);
        self.custom_naming_notes = normalize_naming_notes(&self.custom_naming_notes);
    }

    /// Model path used for escalation attempts in this runtime.
    ///
    /// Keep the configured 1.7B path intact so Settings and the downloader
    /// retain the operator's intent. Only the runtime model selection falls
    /// back to the primary weights while the optional file is absent.
    pub fn effective_escalation_gguf(&self) -> &Path {
        if self.slm_escalation_gguf.is_file() {
            &self.slm_escalation_gguf
        } else {
            &self.slm_primary_gguf
        }
    }

    pub fn using_primary_for_escalation(&self) -> bool {
        !self.slm_escalation_gguf.is_file() && self.slm_primary_gguf.is_file()
    }

    /// The evidence budget for the third naming attempt: 1.6x the configured
    /// one, clamped to `max_evidence_token_budget()`. 4000 at the default 2500.
    ///
    /// Derived rather than configured, deliberately. Rung 3 exists to give a
    /// larger model a *wider* view of the same document, so its budget has to
    /// stay above rung 1's — and two independent fields would let a
    /// hand-edited config invert them, turning the escalation into a narrower
    /// look at a document that has already failed twice, which is the one
    /// thing it must never be. One visible knob scales both rungs, and the
    /// ceiling is what keeps a slot overflow structurally impossible even with
    /// that knob wound to its maximum.
    pub fn escalation_evidence_token_budget(&self) -> usize {
        (self.evidence_token_budget.saturating_mul(8) / 5).min(max_evidence_token_budget())
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Never truncate the live config in place. A power loss or an
        // antivirus scan during a direct write used to leave the next launch
        // with an empty file and therefore a silently reset configuration.
        // The temporary file is fully synced before it replaces the target.
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("backlog.config.json");
        let temp_path = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&temp_path);
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        let write_result = (|| -> anyhow::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }

        #[cfg(not(windows))]
        let replace_result = std::fs::rename(&temp_path, path).map_err(anyhow::Error::from);

        #[cfg(windows)]
        let replace_result = {
            let backup_path = backup_path(path);
            let had_previous = path.exists();
            (|| -> anyhow::Result<()> {
                let _ = std::fs::remove_file(&backup_path);
                if had_previous {
                    std::fs::rename(path, &backup_path)?;
                }
                if let Err(error) = std::fs::rename(&temp_path, path) {
                    if had_previous {
                        let _ = std::fs::rename(&backup_path, path);
                    }
                    return Err(error.into());
                }
                if had_previous {
                    let _ = std::fs::remove_file(&backup_path);
                }
                Ok(())
            })()
        };

        if let Err(error) = replace_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }
        Ok(())
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.outbox_dir.join("_manifests")
    }

    pub fn active_output_dir(&self) -> &Path {
        match self.output_mode {
            OutputMode::PowerAutomate => &self.outbox_dir,
            OutputMode::Local => &self.local_output_dir,
        }
    }

    /// Check an output root stored on an already-ingested job before using it.
    /// Settings validation only sees current roots; recovery also has to reject
    /// an old pinned root that has since become a protected tree or a reparse
    /// path. The active root of the same delivery mode is allowed so ordinary
    /// jobs remain recoverable after a restart without a settings change.
    pub fn validate_pinned_delivery_root(
        &self,
        delivery_mode: &str,
        root: &Path,
    ) -> Result<(), String> {
        if root.as_os_str().is_empty() {
            return Err("pinned delivery root is empty".into());
        }
        if contains_reparse_point(root)
            || (delivery_mode == "power_automate"
                && contains_reparse_point(&root.join("_manifests")))
        {
            return Err("pinned delivery root contains a symlink or Windows reparse point".into());
        }
        let protected: Vec<(&str, &Path)> = match delivery_mode {
            "local" => vec![
                ("Processing", &self.processing_dir),
                ("Outbox", &self.outbox_dir),
                ("Quarantine", &self.quarantine_dir),
                ("Cache", &self.cache_dir),
            ],
            "power_automate" => vec![
                ("Processing", &self.processing_dir),
                ("Local Output", &self.local_output_dir),
                ("Quarantine", &self.quarantine_dir),
                ("Cache", &self.cache_dir),
            ],
            _ => return Err("invalid pinned delivery mode".into()),
        };
        for (name, protected_root) in protected {
            if !protected_root.as_os_str().is_empty() && paths_overlap(root, protected_root) {
                return Err(format!(
                    "pinned delivery root overlaps protected {name} folder"
                ));
            }
        }
        Ok(())
    }

    pub fn ready(&self) -> bool {
        !self.processing_dir.as_os_str().is_empty()
            && !self.active_output_dir().as_os_str().is_empty()
            && !self.quarantine_dir.as_os_str().is_empty()
    }

    /// Reject configurations that would corrupt processing: unset folders,
    /// duplicate folders, or folders nested inside one another. The watcher is
    /// recursive over the processing dir, so a nested outbox/cache/quarantine
    /// would feed the app's own manifests and cached markdown back into the
    /// pipeline as if they were intake documents.
    pub fn validate(&self) -> Result<(), String> {
        if !self.ready() {
            return Err(match self.output_mode {
                OutputMode::PowerAutomate => {
                    "Set the Processing, Outbox, and Quarantine folders first."
                }
                OutputMode::Local => {
                    "Set the Processing, Local Output, and Quarantine folders first."
                }
            }
            .into());
        }
        // `SlmLane` binds `llama_port` and `llama_port + 1`, so the top of the
        // range is not merely unusable — it overflows the u16 add. Reject it
        // here, where the value is entered, rather than at spawn time.
        if self.llama_port < 1024 || self.llama_port == u16::MAX {
            return Err(format!(
                "The llama-server port must be between 1024 and {}; {} is not usable.",
                u16::MAX - 1,
                self.llama_port
            ));
        }
        if !(1..=4).contains(&self.slm_parallel) {
            return Err(format!(
                "slm_parallel must be between 1 and 4; got {}.",
                self.slm_parallel
            ));
        }
        if !(1..=4).contains(&self.slm_escalation_parallel) {
            return Err(format!(
                "slm_escalation_parallel must be between 1 and 4; got {}.",
                self.slm_escalation_parallel
            ));
        }
        if self.slm_recycle_after_requests > 100_000 {
            return Err(format!(
                "slm_recycle_after_requests must be 0 or at most 100000; got {}.",
                self.slm_recycle_after_requests
            ));
        }
        if self.slm_escalation_idle_secs > 86_400 {
            return Err(format!(
                "slm_escalation_idle_secs must be 0 or at most 86400; got {}.",
                self.slm_escalation_idle_secs
            ));
        }
        let max_evidence = max_evidence_token_budget();
        if self.evidence_token_budget == 0 || self.evidence_token_budget > max_evidence {
            return Err(format!(
                "evidence_token_budget must be between 1 and {max_evidence}; got {}. Larger \
                 values would not fit one llama-server slot alongside the prompt and the answer.",
                self.evidence_token_budget
            ));
        }
        // Trimmed length, so padding whitespace never fails a value the
        // operator sees as short enough. The cap keeps the operator section a
        // subordinate note — the system prompt is re-sent on every naming
        // attempt, so unbounded notes would tax every document named.
        let naming_notes_chars = self.custom_naming_notes.trim().chars().count();
        if naming_notes_chars > 600 {
            return Err(format!(
                "custom naming notes must be at most 600 characters; got {naming_notes_chars}."
            ));
        }
        if self.convert_workers == 0 || self.convert_workers > 8 {
            return Err(format!(
                "convert_workers must be between 1 and 8; got {}.",
                self.convert_workers
            ));
        }
        if !(1..=8).contains(&self.convert_min_idle_workers) {
            return Err(format!(
                "convert_min_idle_workers must be between 1 and 8; got {}.",
                self.convert_min_idle_workers
            ));
        }
        if self.convert_idle_reap_secs > 3_600 {
            return Err(format!(
                "convert_idle_reap_secs must be 0 or at most 3600; got {}.",
                self.convert_idle_reap_secs
            ));
        }
        if self.sidecar_timeout_secs == 0 || self.sidecar_timeout_secs > 300 {
            return Err(format!(
                "sidecar_timeout_secs must be between 1 and 300; got {}.",
                self.sidecar_timeout_secs
            ));
        }
        if self.manifest_emit_per_min > 100_000 {
            return Err(format!(
                "manifest_emit_per_min must be 0 or at most 100000; got {}.",
                self.manifest_emit_per_min
            ));
        }
        if self.max_head_pages == 0 || self.max_head_pages > 1_000 {
            return Err(format!(
                "max_head_pages must be between 1 and 1000; got {}.",
                self.max_head_pages
            ));
        }
        if self.max_tail_pages > 1_000 {
            return Err(format!(
                "max_tail_pages must be at most 1000; got {}.",
                self.max_tail_pages
            ));
        }
        if self.max_filename_len < 32 || self.max_filename_len > 240 {
            return Err(format!(
                "max_filename_len must be between 32 and 240; got {}.",
                self.max_filename_len
            ));
        }
        if self.max_stage_attempts == 0 || self.max_stage_attempts > 10 {
            return Err(format!(
                "max_stage_attempts must be between 1 and 10; got {}.",
                self.max_stage_attempts
            ));
        }
        if self.per_file_wall_clock_secs == 0 || self.per_file_wall_clock_secs > 3_600 {
            return Err(format!(
                "per_file_wall_clock_secs must be between 1 and 3600; got {}.",
                self.per_file_wall_clock_secs
            ));
        }
        if self.cache_ttl_days == 0 || self.cache_ttl_days > 3_650 {
            return Err(format!(
                "cache_ttl_days must be between 1 and 3650; got {}.",
                self.cache_ttl_days
            ));
        }
        // A configured but inactive Outbox remains a protected root.  In
        // particular, Local Output must never point into its `_manifests`
        // trigger tree where a native receipt could wake Power Automate.
        let named: [(&str, &Path); 5] = [
            ("Processing", self.processing_dir.as_path()),
            ("Outbox", self.outbox_dir.as_path()),
            ("Local Output", self.local_output_dir.as_path()),
            ("Quarantine", self.quarantine_dir.as_path()),
            ("Cache", self.cache_dir.as_path()),
        ];
        for i in 0..named.len() {
            let (a_name, a_path) = named[i];
            if a_path.as_os_str().is_empty() {
                continue;
            }
            if contains_reparse_point(a_path) {
                return Err(format!(
                    "{a_name} folder contains a symlink or Windows reparse point; choose a real folder."
                ));
            }
            let a = a_path;
            for (b_name, b_path) in named.iter().skip(i + 1) {
                if b_path.as_os_str().is_empty() {
                    continue;
                }
                if paths_overlap(a, b_path) {
                    if path_key(a) == path_key(b_path) {
                        return Err(format!("{a_name} and {b_name} folders must be different."));
                    }
                    return Err(format!(
                        "{a_name} and {b_name} folders cannot be nested inside one another."
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(proc: &str, out: &str, quar: &str, cache: &str) -> Config {
        Config {
            processing_dir: proc.into(),
            outbox_dir: out.into(),
            quarantine_dir: quar.into(),
            cache_dir: cache.into(),
            ..Default::default()
        }
    }

    #[test]
    fn accepts_distinct_folders() {
        assert!(cfg("/a/proc", "/a/out", "/a/quar", "/a/cache")
            .validate()
            .is_ok());
    }

    #[test]
    fn rejects_nested_outbox_under_processing() {
        let c = cfg("/a/proc", "/a/proc/out", "/a/quar", "/a/cache");
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_folders() {
        let c = cfg("/a/proc", "/a/proc", "/a/quar", "/a/cache");
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_unset_folders() {
        assert!(cfg("", "", "", "").validate().is_err());
    }

    #[test]
    fn rejects_a_llama_port_whose_escalation_neighbour_overflows() {
        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.llama_port = u16::MAX;
        assert!(c.validate().is_err());
        c.llama_port = 80;
        assert!(c.validate().is_err());
        c.llama_port = 8137;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_zero_and_unbounded_resource_or_retry_values() {
        let mut cases = Vec::new();

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.slm_parallel = 0;
        cases.push((c, "slm_parallel"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.slm_escalation_parallel = 0;
        cases.push((c, "slm_escalation_parallel"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.slm_escalation_parallel = 5;
        cases.push((c, "slm_escalation_parallel"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.slm_recycle_after_requests = 100_001;
        cases.push((c, "slm_recycle_after_requests"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.slm_escalation_idle_secs = 86_401;
        cases.push((c, "slm_escalation_idle_secs"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.evidence_token_budget = 0;
        cases.push((c, "evidence_token_budget"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.evidence_token_budget = max_evidence_token_budget() + 1;
        cases.push((c, "evidence_token_budget"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.convert_workers = 0;
        cases.push((c, "convert_workers"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.convert_min_idle_workers = 0;
        cases.push((c, "convert_min_idle_workers"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.convert_min_idle_workers = 9;
        cases.push((c, "convert_min_idle_workers"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.convert_idle_reap_secs = 3_601;
        cases.push((c, "convert_idle_reap_secs"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.sidecar_timeout_secs = 0;
        cases.push((c, "sidecar_timeout_secs"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.max_head_pages = 0;
        cases.push((c, "max_head_pages"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.max_filename_len = 0;
        cases.push((c, "max_filename_len"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.max_stage_attempts = 0;
        cases.push((c, "max_stage_attempts"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.per_file_wall_clock_secs = 0;
        cases.push((c, "per_file_wall_clock_secs"));

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.cache_ttl_days = 0;
        cases.push((c, "cache_ttl_days"));

        for (cfg, field) in cases {
            let error = cfg.validate().expect_err(field);
            assert!(error.contains(field), "{field}: {error}");
        }

        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.manifest_emit_per_min = u32::MAX;
        let error = c.validate().expect_err("manifest_emit_per_min");
        assert!(error.contains("manifest_emit_per_min"), "{error}");
    }

    /// The evidence ceiling is what makes a slot overflow structurally
    /// impossible, so it is asserted twice over: once against the literal 4347
    /// that the doc comments and the Settings range are written against, and
    /// once against the slot arithmetic itself, so that a change to
    /// `SLM_CTX_PER_SLOT` fails here rather than silently redefining what
    /// "fits" means.
    #[test]
    fn the_evidence_budgets_always_fit_one_llama_server_slot() {
        let max = max_evidence_token_budget();
        assert_eq!(max, 4347, "the constants no longer produce the pinned 4347");

        // The ceiling the bundle builder actually enforces, in characters.
        // The conversion back has to land inside it: integer division is what
        // makes that true rather than merely likely, and this is where a
        // change to the budget unit would surface.
        let slot_chars = max_bundle_chars();
        assert!(
            max * BUDGET_CHARS_PER_TOKEN <= slot_chars,
            "the ceiling itself must fit the slot it was derived from"
        );
        // And the slot arithmetic behind it, so a change to `SLM_CTX_PER_SLOT`
        // fails here rather than silently redefining what "fits" means.
        assert_eq!(
            slot_chars,
            (SLM_CTX_PER_SLOT
                - crate::slm::SLM_PROMPT_RESERVE_TOKENS
                - crate::slm::SLM_MAX_OUTPUT_TOKENS) as usize
                * crate::filter::CONSERVATIVE_CHARS_PER_TOKEN
        );

        // Rung 3 is 1.6x rung 1, and the default pair is the shape the
        // pipeline was tuned on.
        let cfg = Config::default();
        assert_eq!(cfg.evidence_token_budget, 2500);
        assert_eq!(cfg.escalation_evidence_token_budget(), 4000);
        assert!(cfg.escalation_evidence_token_budget() > cfg.evidence_token_budget);

        // The escalation budget is clamped, not merely scaled, so even the
        // largest configuration `validate` accepts cannot overflow a slot.
        let widest = Config {
            evidence_token_budget: max,
            ..Default::default()
        };
        assert_eq!(widest.escalation_evidence_token_budget(), max);
        assert!(widest.escalation_evidence_token_budget() * BUDGET_CHARS_PER_TOKEN <= slot_chars);

        // A tiny budget still scales rather than collapsing to the floor.
        let narrow = Config {
            evidence_token_budget: 400,
            ..Default::default()
        };
        assert_eq!(narrow.escalation_evidence_token_budget(), 640);
    }

    /// Unlike every other duration/count knob above, 0 is a legal value here
    /// on purpose: it is how idle reaping is turned off, matching the
    /// pre-feature behavior where the pool only ever grew.
    #[test]
    fn convert_idle_reap_secs_zero_disables_reaping_and_is_valid() {
        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.convert_idle_reap_secs = 0;
        assert!(c.validate().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_configured_root_that_contains_a_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-processing");
        let link = dir.path().join("processing-link");
        std::fs::create_dir_all(&real).unwrap();
        symlink(&real, &link).unwrap();

        let c = cfg(
            link.to_str().unwrap(),
            dir.path().join("out").to_str().unwrap(),
            dir.path().join("quar").to_str().unwrap(),
            dir.path().join("cache").to_str().unwrap(),
        );
        let error = c.validate().expect_err("symlinked roots must be rejected");
        assert!(
            error.contains("reparse") || error.contains("symlink"),
            "{error}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_a_configured_root_that_contains_a_junction() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-processing");
        let junction = dir.path().join("processing-junction");
        std::fs::create_dir_all(&real).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&real)
            .status()
            .unwrap();
        assert!(status.success(), "could not create junction for test");

        let c = cfg(
            junction.to_str().unwrap(),
            dir.path().join("out").to_str().unwrap(),
            dir.path().join("quar").to_str().unwrap(),
            dir.path().join("cache").to_str().unwrap(),
        );
        let error = c.validate().expect_err("junction roots must be rejected");
        assert!(
            error.contains("reparse") || error.contains("junction"),
            "{error}"
        );
    }

    #[test]
    fn normalize_strips_quotes_and_padding_from_every_path_field() {
        // Exactly what Explorer's "Copy as path" pastes, plus the stray
        // spaces a hand-edited config picks up.
        let mut c = cfg(
            "  \"C:\\Users\\z\\Processing\"  ",
            " 'D:/Outbox' ",
            "C:\\Quarantine ",
            "\"C:\\Cache\"",
        );
        c.slm_primary_gguf = " \"C:\\models\\a.gguf\" ".into();
        c.ettin_model_dir = "  \"C:\\ettin\"  ".to_string();
        c.normalize();

        assert_eq!(c.processing_dir, PathBuf::from("C:\\Users\\z\\Processing"));
        assert_eq!(c.outbox_dir, PathBuf::from("D:/Outbox"));
        assert_eq!(c.quarantine_dir, PathBuf::from("C:\\Quarantine"));
        assert_eq!(c.cache_dir, PathBuf::from("C:\\Cache"));
        assert_eq!(c.slm_primary_gguf, PathBuf::from("C:\\models\\a.gguf"));
        assert_eq!(c.ettin_model_dir, "C:\\ettin");
    }

    #[test]
    fn normalize_leaves_an_unquoted_path_and_a_lone_quote_alone() {
        let mut c = cfg("/a/proc", "/a/o\"ut", "/a/quar", "");
        c.normalize();
        assert_eq!(c.processing_dir, PathBuf::from("/a/proc"));
        // Only a *matched* surrounding pair is stripped; an interior quote is
        // a legal (if unwise) filename character and must survive.
        assert_eq!(c.outbox_dir, PathBuf::from("/a/o\"ut"));
        assert_eq!(c.cache_dir, PathBuf::from(""));
    }

    #[test]
    fn load_normalizes_a_hand_edited_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backlog.config.json");
        std::fs::write(
            &path,
            r#"{"processing_dir":"  \"C:\\Processing\"  ","outbox_dir":"C:\\Outbox"}"#,
        )
        .unwrap();
        let cfg = Config::load(&path);
        assert_eq!(cfg.processing_dir, PathBuf::from("C:\\Processing"));
        assert_eq!(cfg.outbox_dir, PathBuf::from("C:\\Outbox"));
    }

    #[test]
    fn load_tolerates_a_utf8_bom_on_the_main_file() {
        let dir = tempfile::tempdir().unwrap();
        let bommed = dir.path().join("bommed.config.json");
        let plain = dir.path().join("plain.config.json");
        let body = r#"{"processing_dir":"C:\\Processing","llama_port":9999}"#;
        let mut bommed_bytes = vec![0xEF, 0xBB, 0xBF];
        bommed_bytes.extend_from_slice(body.as_bytes());
        std::fs::write(&bommed, &bommed_bytes).unwrap();
        std::fs::write(&plain, body).unwrap();

        let from_bom = Config::load(&bommed);
        let from_plain = Config::load(&plain);

        assert_eq!(from_bom.processing_dir, from_plain.processing_dir);
        assert_eq!(from_bom.processing_dir, PathBuf::from("C:\\Processing"));
        assert_eq!(from_bom.llama_port, from_plain.llama_port);
        assert_eq!(from_bom.llama_port, 9999);
        assert!(
            !invalid_path(&bommed).exists(),
            "a BOM alone must not be treated as a parse failure"
        );
    }

    #[test]
    fn load_preserves_an_unparseable_main_file_instead_of_losing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backlog.config.json");
        let garbage = b"{ not valid json at all".to_vec();
        std::fs::write(&path, &garbage).unwrap();

        let loaded = Config::load(&path);

        // No backup was present, so defaults win.
        assert_eq!(loaded.processing_dir, PathBuf::new());

        let invalid = invalid_path(&path);
        assert!(
            invalid.is_file(),
            "the unparseable file must be preserved at <path>.invalid"
        );
        assert_eq!(std::fs::read(&invalid).unwrap(), garbage);

        assert!(
            path.is_file(),
            "the original file must still be on disk, untouched"
        );
        assert_eq!(std::fs::read(&path).unwrap(), garbage);
    }

    #[test]
    fn load_preserves_an_unparseable_main_file_even_when_a_backup_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backlog.config.json");
        let backup = backup_path(&path);
        let garbage = b"{ not valid json at all".to_vec();
        std::fs::write(&path, &garbage).unwrap();
        let expected = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        std::fs::write(&backup, serde_json::to_vec(&expected).unwrap()).unwrap();

        let loaded = Config::load(&path);

        assert_eq!(loaded.processing_dir, expected.processing_dir);

        let invalid = invalid_path(&path);
        assert!(
            invalid.is_file(),
            "the unparseable main file must be preserved even though the backup recovered"
        );
        assert_eq!(std::fs::read(&invalid).unwrap(), garbage);
    }

    #[test]
    fn load_recovers_a_bommed_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backlog.config.json");
        let backup = backup_path(&path);
        let expected = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(&serde_json::to_vec(&expected).unwrap());
        std::fs::write(&backup, &bytes).unwrap();

        let loaded = Config::load(&path);

        assert_eq!(loaded.processing_dir, expected.processing_dir);
        assert_eq!(loaded.outbox_dir, expected.outbox_dir);
        assert!(
            path.is_file(),
            "a recovered BOM'd backup should restore the live path"
        );
    }

    #[test]
    fn load_recovers_a_complete_backup_when_the_live_config_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backlog.config.json");
        let backup = backup_path(&path);
        let expected = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        std::fs::write(&backup, serde_json::to_vec(&expected).unwrap()).unwrap();

        let loaded = Config::load(&path);

        assert_eq!(loaded.processing_dir, expected.processing_dir);
        assert_eq!(loaded.outbox_dir, expected.outbox_dir);
        assert!(
            path.is_file(),
            "a recovered backup should restore the live path"
        );
        assert!(
            !backup.exists(),
            "the recovered backup should not be replayed twice"
        );
    }

    /// `slm_parallel` is a memory knob: `slm.rs` derives `--ctx-size` as
    /// `SLM_CTX_PER_SLOT * slm_parallel` and llama.cpp preallocates the whole
    /// KV cache, at a calculated 728 MiB per slot for the 28-layer primary —
    /// beside, not instead of, the 2977 MiB the 1.7B escalation server holds
    /// once it wakes.
    ///
    /// 14 GiB is the row that matters most: it is what `total_ram_gib()`
    /// reports on the ~14.7 GB laptop this ships to, and it must land in the
    /// 16 GB class rather than the workstation one. The 8 GiB row is the other
    /// row a large build machine can never produce, so both are asserted here.
    #[test]
    fn slm_parallel_is_chosen_from_installed_ram() {
        for (gib, expected) in [
            (4u64, 1u8),
            (8, 1),
            (9, 1),
            (12, 1),
            (14, 1), // the deployment target
            (16, 1),
            (17, 1),
            (32, 2),
        ] {
            assert_eq!(
                slm_parallel_for_ram(Some(gib)),
                expected,
                "{gib} GiB should give {expected}"
            );
        }
        // Unknown RAM must not gamble on behalf of the smaller machine.
        assert_eq!(slm_parallel_for_ram(None), 1);
    }

    /// One escalation slot on every machine. Escalation is the rare third
    /// attempt at a document that already failed twice, so a second slot buys
    /// overlap that never happens — and what it would cost is another 728 MiB
    /// of the 1.7B's KV cache, taking the 16 GB class from a calculated
    /// 4815 MiB to 5543 MiB for concurrency it cannot use.
    #[test]
    fn slm_escalation_parallel_is_one_on_every_machine() {
        for gib in [4u64, 8, 9, 12, 14, 16, 17, 32, 128] {
            assert_eq!(
                slm_escalation_parallel_for_ram(Some(gib)),
                1,
                "{gib} GiB should give 1"
            );
        }
        assert_eq!(slm_escalation_parallel_for_ram(None), 1);
    }

    /// The model tiers, which are memory decisions as much as `slm_parallel`
    /// is. Three invariants beyond the table itself: the small tiers collapse
    /// *exactly* (`slm_escalation_gguf == slm_primary_gguf` is what
    /// `SlmLane::escalation_collapsed()` tests, so "equivalent" paths would
    /// not do); the primary is the same 0.6B on every row, including the
    /// 128 GiB one, because it was chosen on measured quality-per-second and
    /// not on what a machine can afford (see `default_primary_gguf_for_ram`);
    /// and no tier at any size names the 4B, which stays in `GGUF_SHAPES` for
    /// sizing only.
    ///
    /// The constants are pinned to their literal paths here on purpose. Every
    /// other assertion in this file compares one constant against another and
    /// would keep passing if both moved together, which is exactly how a tier
    /// change gets re-introduced by editing two lines.
    #[test]
    fn models_are_chosen_from_installed_ram() {
        assert_eq!(DEFAULT_PRIMARY_GGUF, "models/Qwen3-0.6B-Q8_0.gguf");
        assert_eq!(DEFAULT_ESCALATION_GGUF, "models/Qwen3-1.7B-Q8_0.gguf");

        for (gib, escalation) in [
            (Some(4u64), None),
            (Some(8), None),
            // The collapse boundary, asserted from both sides.
            (Some(9), None),
            (Some(10), Some(DEFAULT_ESCALATION_GGUF)),
            (Some(12), Some(DEFAULT_ESCALATION_GGUF)),
            // The deployment target: 0.6B primary, real 1.7B escalation.
            (Some(14), Some(DEFAULT_ESCALATION_GGUF)),
            (Some(17), Some(DEFAULT_ESCALATION_GGUF)),
            (Some(32), Some(DEFAULT_ESCALATION_GGUF)),
            (Some(128), Some(DEFAULT_ESCALATION_GGUF)),
            // A machine that will not say gets the smallest tier's answer.
            (None, None),
        ] {
            let chosen_primary = default_primary_gguf_for_ram(gib);
            let chosen_escalation = default_escalation_gguf_for_ram(gib);
            assert_eq!(
                chosen_primary, DEFAULT_PRIMARY_GGUF,
                "every tier gets the same primary; {gib:?} GiB did not"
            );
            assert_ne!(
                chosen_escalation, "models/Qwen3-4B-Q4_K_M.gguf",
                "no tier defaults to the 4B; {gib:?} GiB did"
            );
            match escalation {
                Some(expected) => {
                    assert_eq!(chosen_escalation, expected, "escalation for {gib:?} GiB");
                    assert!(
                        separate_escalation_model_fits(gib),
                        "{gib:?} GiB should get its own escalation server"
                    );
                }
                None => {
                    assert_eq!(
                        chosen_escalation, chosen_primary,
                        "{gib:?} GiB must collapse onto the exact primary path"
                    );
                    assert!(!separate_escalation_model_fits(gib));
                }
            }
        }
    }

    /// Every path a tier can produce has to be one the shape table knows,
    /// or the RAM estimates for a stock install silently fall back to the
    /// pessimistic unknown-model path.
    #[test]
    fn the_default_models_are_all_in_the_shape_table() {
        for gib in [Some(8u64), Some(14), Some(32), None] {
            for path in [
                default_primary_gguf_for_ram(gib),
                default_escalation_gguf_for_ram(gib),
            ] {
                assert!(
                    shape_for(Path::new(path)).is_some(),
                    "{path} is not in GGUF_SHAPES"
                );
            }
        }
        let cfg = Config::default();
        assert!(shape_for(&cfg.slm_primary_gguf).is_some());
        assert!(shape_for(&cfg.slm_escalation_gguf).is_some());
        // And the stock config is a valid one on any machine.
        assert!(cfg.evidence_token_budget <= max_evidence_token_budget());
    }

    /// The gotcha this table exists for: the 4B has 36 layers, not the 28 the
    /// old hardcoded estimate assumed, so its KV cache costs 1.286x more per
    /// token. Anything that hardcodes one shape is wrong for the other — and
    /// that is now easier to get wrong, not harder, because both *shipped*
    /// models are 28-layer and a hardcoded 28 would pass every default.
    #[test]
    fn kv_cache_math_matches_the_pinned_model_shapes() {
        assert_eq!(kv_bytes_per_token(28, 8, 128), 114_688);
        assert_eq!(kv_bytes_per_token(36, 8, 128), 147_456);
        for (basename, per_token, mib_per_slot) in [
            ("Qwen3-0.6B-Q8_0.gguf", 114_688u64, 728u64),
            ("Qwen3-1.7B-Q8_0.gguf", 114_688, 728),
            ("Qwen3-4B-Q4_K_M.gguf", 147_456, 936),
        ] {
            let shape = shape_for(Path::new(basename)).expect(basename);
            assert_eq!(
                kv_bytes_per_token(shape.layers, shape.kv_heads, shape.head_dim),
                per_token,
                "{basename}"
            );
            assert_eq!(
                per_token * SLM_CTX_PER_SLOT as u64 / (1024 * 1024),
                mib_per_slot,
                "{basename} per-slot KV"
            );
        }
    }

    /// The footprints the tier doc comments quote are calculated, and this is
    /// the calculation — asserted so the prose and the arithmetic cannot drift
    /// apart the way the old "448 MiB per slot" comment did. Every literal
    /// here appears verbatim in `default_slm_parallel`'s table or in the
    /// comment of the knob that cites it.
    ///
    /// These were recomputed for this pair rather than restored: the slot
    /// moved 4096 -> 6656 and `GgufShape::overhead_bytes` replaced a flat
    /// 200 MiB, so no footprint predating either change survives, including
    /// for models that did not change.
    #[test]
    fn the_naming_lane_footprint_matches_the_sizing_table() {
        let primary = shape_for(Path::new("Qwen3-0.6B-Q8_0.gguf")).unwrap();
        let escalation = shape_for(Path::new("Qwen3-1.7B-Q8_0.gguf")).unwrap();
        // Catalogued but no longer a default; see `GGUF_SHAPES`.
        let unknown_ceiling = shape_for(Path::new("Qwen3-4B-Q4_K_M.gguf")).unwrap();

        assert_eq!(resident_mib(primary, 1), 1838); // every tier's primary
        assert_eq!(resident_mib(primary, 2), 2566); // > 17 GiB primary
        assert_eq!(resident_mib(escalation, 1), 2977); // escalation server

        // The two-server transient worst cases the budgets are built on.
        assert_eq!(resident_mib(primary, 1) + resident_mib(escalation, 1), 4815);
        assert_eq!(resident_mib(primary, 2) + resident_mib(escalation, 1), 5543);

        // A second escalation slot costs the same 728 MiB and buys nothing;
        // `slm_escalation_parallel_for_ram` quotes this pair of numbers as the
        // price of the thing it refuses to do.
        assert_eq!(resident_mib(escalation, 2), 3705);
        assert_eq!(resident_mib(primary, 1) + resident_mib(escalation, 2), 5543);

        // The per-slot KV term each row is broken into, asserted as the
        // difference it actually is rather than as a number retyped from
        // `kv_bytes_per_token`.
        assert_eq!(resident_mib(primary, 2) - resident_mib(primary, 1), 728);
        assert_eq!(
            resident_mib(escalation, 2) - resident_mib(escalation, 1),
            728
        );

        // What an unknown or hand-configured GGUF is charged instead — the
        // reason the 4B stays catalogued after being dropped as a default.
        // `shape_or_largest` and every budget comparison built on it lean on
        // this staying the largest thing the table has weighed.
        assert_eq!(resident_mib(unknown_ceiling, 1), 5218);
        assert_eq!(
            resident_mib(unknown_ceiling, 1) - resident_mib(escalation, 1),
            2241,
            "dropping the 4B would make the pessimistic fallback 2241 MiB cheaper"
        );
    }

    /// An operator may point Settings at any GGUF. We cannot know its shape,
    /// so the estimate degrades to the largest model we do know — never the
    /// smallest. Being too pessimistic wastes a slot; being too optimistic
    /// wedges the machine.
    ///
    /// This is the test that makes retiring the 4B as a *default* safe.
    /// `largest_known_shape` folds `GGUF_SHAPES`, not the default table, so a
    /// model can stop being shipped without stopping being a ceiling — and
    /// nothing here should ever resolve to a model a tier actually names.
    #[test]
    fn an_unknown_gguf_is_sized_as_the_largest_known_shape() {
        let unknown = Path::new("C:/models/somebody-elses-70b.gguf");
        assert!(shape_for(unknown).is_none());

        let assumed = shape_or_largest(unknown);
        assert_eq!(assumed.basename, "Qwen3-4B-Q4_K_M.gguf");
        for shape in GGUF_SHAPES {
            assert!(
                resident_bytes(assumed, 1) >= resident_bytes(shape, 1),
                "the fallback must not be cheaper than {}",
                shape.basename
            );
        }
        // The ceiling outlives the defaults: no tier may name the model that
        // is standing in for "we have no idea how big this is".
        for gib in [None, Some(8u64), Some(14), Some(64)] {
            for path in [
                default_primary_gguf_for_ram(gib),
                default_escalation_gguf_for_ram(gib),
            ] {
                assert_ne!(
                    shape_or_largest(Path::new(path)).basename,
                    assumed.basename,
                    "{path} is a default AND the pessimistic fallback"
                );
            }
        }

        // A hand-configured 4B is priced at its own shape rather than
        // mistaken for the 1.7B it sits above in the catalog. That is what
        // lets the budget see the real cost of the lane it sits in — and, the
        // other half of the same coin, what makes it replaceable at all.
        let by_hand = Path::new("D:/models/Qwen3-4B-Q4_K_M.gguf");
        assert_eq!(resident_mib(shape_or_largest(by_hand), 1), 5218);
        assert!(model_is_replaceable(by_hand));
        assert!(
            !model_is_replaceable(unknown),
            "an unpriceable model must never be swapped out from under the operator"
        );

        // Matching is on the file name and Windows is case-insensitive.
        assert_eq!(
            shape_or_largest(Path::new("C:/Models/qwen3-1.7b-q8_0.gguf")).basename,
            "Qwen3-1.7B-Q8_0.gguf"
        );
    }

    /// Each `convertd` worker converges toward `CONVERTD_WORKER_RSS_MB`
    /// (550 MB, measured with OCR+lingua both loaded) now that `Sidecar`
    /// pools them, so two of them is 1.1 GB and leaves under 150 MB of
    /// margin on an 8 GB machine beside Windows, the model servers and the
    /// app — the 8 GB tier drops from 2 workers to 1 for exactly that reason.
    ///
    /// The 16 GB tier drops from 4 to 3 for a different reason, and not a
    /// pure memory one: at rest the pair holds a calculated 4815 MiB and four
    /// workers would fit. What buys the fourth worker back is that at-rest is
    /// not steady state (the 1.7B measured 2,860 -> 5,785 MiB over 16 requests
    /// at this context) and that `Sidecar` serializes conversions, so the
    /// fourth would be real memory spent on throughput the pipeline cannot
    /// take. See `convert_workers_ram_ceiling`.
    #[test]
    fn convert_workers_are_capped_by_installed_ram() {
        for (gib, expected) in [
            (4u64, 1usize),
            (8, 1),
            (9, 1),
            (12, 3),
            (14, 3), // the deployment target
            (17, 3),
            (32, 6),
        ] {
            assert_eq!(
                convert_workers_ram_ceiling(Some(gib)),
                expected,
                "{gib} GiB should cap at {expected}"
            );
        }
        assert_eq!(convert_workers_ram_ceiling(None), 1);
        // The live default must never exceed the ceiling for this machine.
        assert!(default_convert_workers() <= convert_workers_ram_ceiling(total_ram_gib()));
        assert!(default_convert_workers() >= 1, "one worker is the floor");
    }

    /// `backlog.config.json` outlives the installer, so a machine that ran an
    /// older build keeps whatever it wrote. An 8 GB laptop upgrading from the
    /// flat default of 4 would otherwise thrash through its whole backfill
    /// having never chosen that number.
    #[test]
    fn a_persisted_slm_parallel_is_clamped_down_but_never_up() {
        let mut cfg = Config {
            slm_parallel: 4,
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(8));
        assert_eq!(cfg.slm_parallel, 1, "8 GB must not inherit 4");

        // The deployment target keeps one slot too — and not because it
        // cannot afford two (5543 MiB fits). The measured throughput this
        // build ships against was taken at `slm_parallel: 1`, and nothing has
        // measured the second slot.
        let mut cfg = Config {
            slm_parallel: 2,
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(14));
        assert_eq!(cfg.slm_parallel, 1, "14 GB must not inherit 2");

        // One-directional: someone who lowered it knows their machine.
        let mut cfg = Config {
            slm_parallel: 1,
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(64));
        assert_eq!(cfg.slm_parallel, 1, "a deliberate 1 must survive on 64 GB");
    }

    /// Same one-directional contract for the escalation tier's own knob.
    #[test]
    fn a_persisted_slm_escalation_parallel_is_clamped_down_but_never_up() {
        let mut cfg = Config {
            slm_escalation_parallel: 2,
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(8));
        assert_eq!(cfg.slm_escalation_parallel, 1, "8 GB must not inherit 2");

        // Not even a workstation gets a second escalation slot.
        let mut cfg = Config {
            slm_escalation_parallel: 4,
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(64));
        assert_eq!(cfg.slm_escalation_parallel, 1);

        // One-directional: someone who lowered it knows their machine.
        let mut cfg = Config {
            slm_escalation_parallel: 1,
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(64));
        assert_eq!(
            cfg.slm_escalation_parallel, 1,
            "a deliberate 1 must survive on 64 GB"
        );
    }

    /// The models are persisted too, and they are the expensive half of the
    /// budget. A config written on (or imported from) a larger machine can
    /// name the 1.7B as its primary; an 8 GB machine loading it must come back
    /// down to the bundled 0.6B and a collapsed escalation tier, or it holds a
    /// calculated 2977 MiB against a 2300 MiB budget on a box with about
    /// 7.4 GiB usable.
    ///
    /// What is asserted here is the *order* of the reduction as much as the
    /// outcome. Over-budget pairs give up the second server before they give up
    /// a model, and give up the escalation model before the primary, because
    /// that is the order that costs the operator least: collapse still runs
    /// rung 3, while replacing the primary changes how every document is named.
    #[test]
    fn a_persisted_model_pair_is_clamped_down_but_never_up() {
        // A pair from a machine that could afford a separate escalation
        // server, loaded on one that cannot.
        let mut cfg = Config {
            slm_primary_gguf: DEFAULT_ESCALATION_GGUF.into(),
            slm_escalation_gguf: DEFAULT_ESCALATION_GGUF.into(),
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(8));
        assert_eq!(cfg.slm_primary_gguf, PathBuf::from(DEFAULT_PRIMARY_GGUF));
        assert_eq!(
            cfg.slm_escalation_gguf, cfg.slm_primary_gguf,
            "8 GB must collapse onto the primary it ended up with, exactly"
        );

        // The deployment target keeps the shipped pair — this is the tier the
        // sizing exists for, and nothing about it may be clamped.
        let mut cfg = Config {
            slm_primary_gguf: DEFAULT_PRIMARY_GGUF.into(),
            slm_escalation_gguf: DEFAULT_ESCALATION_GGUF.into(),
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(14));
        assert_eq!(cfg.slm_primary_gguf, PathBuf::from(DEFAULT_PRIMARY_GGUF));
        assert_eq!(
            cfg.slm_escalation_gguf,
            PathBuf::from(DEFAULT_ESCALATION_GGUF)
        );

        // A config left behind by a build of the rejected tier change names
        // the 1.7B/4B pair, a calculated 8195 MiB. On the 16 GB target that is
        // over the 5900 MiB budget, and the cheapest way back under it is to
        // drop the second *server* — the 1.7B primary the operator has is
        // 2977 MiB and fits fine, so it is not the thing that has to go.
        let mut cfg = Config {
            slm_primary_gguf: DEFAULT_ESCALATION_GGUF.into(),
            slm_escalation_gguf: "models/Qwen3-4B-Q4_K_M.gguf".into(),
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(14));
        assert_eq!(
            cfg.slm_primary_gguf,
            PathBuf::from(DEFAULT_ESCALATION_GGUF),
            "a 1.7B primary fits 16 GB and must not be taken away to fix the escalation tier"
        );
        assert_eq!(
            cfg.slm_escalation_gguf, cfg.slm_primary_gguf,
            "the second server is what 16 GB cannot afford, so it collapses"
        );

        // One-directional: an explicitly collapsed pair survives on a machine
        // that could have afforded a second server.
        let mut cfg = Config {
            slm_primary_gguf: DEFAULT_PRIMARY_GGUF.into(),
            slm_escalation_gguf: DEFAULT_PRIMARY_GGUF.into(),
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(64));
        assert_eq!(
            cfg.slm_primary_gguf,
            PathBuf::from(DEFAULT_PRIMARY_GGUF),
            "a deliberate 0.6B must survive on 64 GB"
        );
        assert_eq!(
            cfg.slm_escalation_gguf,
            PathBuf::from(DEFAULT_PRIMARY_GGUF),
            "a deliberately collapsed pair must survive on 64 GB"
        );

        // A machine that will not report its memory is treated as the
        // smallest one, models included.
        let mut cfg = Config {
            slm_primary_gguf: DEFAULT_ESCALATION_GGUF.into(),
            slm_escalation_gguf: DEFAULT_ESCALATION_GGUF.into(),
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(None);
        assert_eq!(cfg.slm_primary_gguf, PathBuf::from(DEFAULT_PRIMARY_GGUF));
        assert_eq!(cfg.slm_escalation_gguf, cfg.slm_primary_gguf);
    }

    /// The property the whole model clamp rests on: whatever a tier ships by
    /// default must fit that tier's own budget, at that tier's own slot counts.
    ///
    /// Step 2 of the clamp falls back to `default_primary_gguf_for_ram` /
    /// `default_escalation_gguf_for_ram` and does not re-check the result,
    /// because it cannot usefully do anything if that fails. This is what makes
    /// that safe. It also fails loudly if someone widens a default, raises
    /// `SLM_CTX_PER_SLOT` again, or lowers a budget without checking the other
    /// two — the failure mode otherwise is a fresh install clamping itself on
    /// first launch and logging a warning about a config nobody wrote.
    #[test]
    fn every_tier_can_afford_its_own_defaults() {
        for gib in [
            None,
            Some(4u64),
            Some(8),
            Some(9),
            Some(10),
            Some(14),
            Some(17),
            Some(32),
        ] {
            let lane = naming_lane_mib(
                Path::new(default_primary_gguf_for_ram(gib)),
                Path::new(default_escalation_gguf_for_ram(gib)),
                slm_parallel_for_ram(gib),
                slm_escalation_parallel_for_ram(gib),
            );
            let budget = naming_lane_budget_mib(gib);
            assert!(
                lane <= budget,
                "{gib:?} GiB defaults hold {lane} MiB against a {budget} MiB budget"
            );
        }

        // And the three tier budgets are the numbers the doc comment derives,
        // so the derivation table cannot drift from the match arms.
        assert_eq!(naming_lane_budget_mib(Some(8)), 2300);
        assert_eq!(naming_lane_budget_mib(Some(14)), 5900);
        assert_eq!(naming_lane_budget_mib(Some(64)), 16600);
        assert_eq!(
            naming_lane_budget_mib(None),
            naming_lane_budget_mib(Some(8)),
            "unknown RAM gets the smallest machine's budget"
        );
    }

    /// The model budget prices the lane at the slot counts this run will
    /// actually use, which means the slot clamps have to have happened first.
    ///
    /// This config is the shipped 16 GB pair — 4815 MiB against a 5900 MiB
    /// budget, comfortably fine — carrying slot counts from a machine that
    /// could afford them. Priced at the persisted 4/4 the same pair reads
    /// 9183 MiB and the models would be needlessly collapsed; priced at the
    /// clamped 1/1 nothing is wrong with it at all. Only the ordering inside
    /// `clamp_resources_for_ram` decides which happens.
    #[test]
    fn the_model_budget_prices_slots_after_they_are_clamped() {
        let mut cfg = Config {
            slm_parallel: 4,
            slm_escalation_parallel: 4,
            slm_primary_gguf: DEFAULT_PRIMARY_GGUF.into(),
            slm_escalation_gguf: DEFAULT_ESCALATION_GGUF.into(),
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(14));

        assert_eq!(cfg.slm_parallel, 1);
        assert_eq!(cfg.slm_escalation_parallel, 1);
        assert_eq!(
            cfg.slm_primary_gguf,
            PathBuf::from(DEFAULT_PRIMARY_GGUF),
            "the slot clamp already fixed the overcommitment; the models were never the problem"
        );
        assert_eq!(
            cfg.slm_escalation_gguf,
            PathBuf::from(DEFAULT_ESCALATION_GGUF),
            "a separate escalation server must survive a purely slot-shaped overcommitment"
        );

        // The two prices this test is discriminating between.
        assert_eq!(
            naming_lane_mib(
                Path::new(DEFAULT_PRIMARY_GGUF),
                Path::new(DEFAULT_ESCALATION_GGUF),
                4,
                4
            ),
            9183
        );
        assert_eq!(
            naming_lane_mib(
                Path::new(DEFAULT_PRIMARY_GGUF),
                Path::new(DEFAULT_ESCALATION_GGUF),
                1,
                1
            ),
            4815
        );
    }

    /// The clamp asks what the machine can hold, not what the tier ships.
    ///
    /// A 4B escalation server is not a default any more, and before this
    /// distinction existed that alone was enough to have it replaced — on a
    /// 64 GiB workstation with over 20 GiB free. It is a real 5218 MiB and a
    /// deliberate choice, and the only thing that justifies overriding it is
    /// the memory not being there.
    #[test]
    fn a_hand_configured_model_survives_wherever_the_machine_can_hold_it() {
        let four_b = "models/Qwen3-4B-Q4_K_M.gguf";
        // 0.6B primary + 4B escalation: a calculated 7056 MiB.
        let lane = naming_lane_mib(Path::new(DEFAULT_PRIMARY_GGUF), Path::new(four_b), 1, 1);
        assert_eq!(lane, 7056);

        // The workstation holds it comfortably, so nothing is touched — not
        // the escalation model, and not the `slm_parallel` the operator was
        // running it at.
        let mut cfg = Config {
            slm_primary_gguf: DEFAULT_PRIMARY_GGUF.into(),
            slm_escalation_gguf: four_b.into(),
            slm_parallel: 1,
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(64));
        assert_eq!(cfg.slm_primary_gguf, PathBuf::from(DEFAULT_PRIMARY_GGUF));
        assert_eq!(
            cfg.slm_escalation_gguf,
            PathBuf::from(four_b),
            "7056 MiB against a 16600 MiB budget is not a reason to override anyone"
        );

        // The 8 GB machine cannot, so it collapses — and lands on a lane it
        // can actually afford rather than merely a smaller one.
        let mut cfg = Config {
            slm_primary_gguf: DEFAULT_PRIMARY_GGUF.into(),
            slm_escalation_gguf: four_b.into(),
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(8));
        assert_eq!(cfg.slm_primary_gguf, PathBuf::from(DEFAULT_PRIMARY_GGUF));
        assert_eq!(
            cfg.slm_escalation_gguf, cfg.slm_primary_gguf,
            "8 GB must collapse onto the exact primary path"
        );
        assert!(
            naming_lane_mib(
                &cfg.slm_primary_gguf,
                &cfg.slm_escalation_gguf,
                cfg.slm_parallel,
                cfg.slm_escalation_parallel,
            ) <= naming_lane_budget_mib(Some(8))
        );
    }

    /// An uncatalogued GGUF is priced pessimistically and replaced never. The
    /// two are not in tension: the pessimistic price can push the *other* half
    /// of the pair down, which is the safe direction, while the model the
    /// operator explicitly named is left exactly where they put it.
    #[test]
    fn an_unpriceable_model_is_sized_pessimistically_and_never_replaced() {
        let mystery = "D:/models/somebody-elses-70b.gguf";

        // Priced as the largest shape we know, so the pair reads 5218 + 2977.
        assert_eq!(
            naming_lane_mib(Path::new(mystery), Path::new(DEFAULT_ESCALATION_GGUF), 1, 1),
            8195
        );

        // On 8 GB that is far over budget, but the operator's primary survives
        // — only the half this module can price is given up.
        let mut cfg = Config {
            slm_primary_gguf: mystery.into(),
            slm_escalation_gguf: DEFAULT_ESCALATION_GGUF.into(),
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(8));
        assert_eq!(
            cfg.slm_primary_gguf,
            PathBuf::from(mystery),
            "a model we cannot price is a model we must not replace"
        );
        assert_eq!(
            cfg.slm_escalation_gguf, cfg.slm_primary_gguf,
            "the priceable half still comes down, which here means collapsing"
        );

        // And when it is the only model named, nothing happens at all: there
        // is no smaller thing to give up that we are entitled to take.
        let mut cfg = Config {
            slm_primary_gguf: mystery.into(),
            slm_escalation_gguf: mystery.into(),
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(8));
        assert_eq!(cfg.slm_primary_gguf, PathBuf::from(mystery));
        assert_eq!(cfg.slm_escalation_gguf, PathBuf::from(mystery));
    }

    /// Every knob at once, from the most overcommitted config a larger machine
    /// could hand an 8 GB one. The pair it lands on is the 1838 MiB collapsed
    /// lane, which is the only naming footprint this tier was ever budgeted
    /// for.
    #[test]
    fn an_eight_gib_machine_clamps_every_process_pool() {
        let mut cfg = Config {
            slm_parallel: 4,
            slm_escalation_parallel: 2,
            convert_workers: 6,
            slm_primary_gguf: DEFAULT_ESCALATION_GGUF.into(),
            slm_escalation_gguf: "models/Qwen3-4B-Q4_K_M.gguf".into(),
            ..Default::default()
        };
        cfg.clamp_resources_for_ram(Some(8));
        assert_eq!(cfg.slm_parallel, 1);
        assert_eq!(cfg.slm_escalation_parallel, 1);
        assert_eq!(cfg.convert_workers, 1);
        assert_eq!(cfg.slm_primary_gguf, PathBuf::from(DEFAULT_PRIMARY_GGUF));
        assert_eq!(cfg.slm_escalation_gguf, cfg.slm_primary_gguf);
        assert_eq!(
            resident_mib(shape_or_largest(&cfg.slm_primary_gguf), cfg.slm_parallel),
            1838,
            "the clamped 8 GB config must land on the footprint its tier budgets"
        );
    }

    #[test]
    fn normalize_preserves_missing_escalation_intent() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("primary.gguf");
        std::fs::write(&primary, b"model").unwrap();
        let mut cfg = Config {
            slm_primary_gguf: primary.clone(),
            slm_escalation_gguf: dir.path().join("missing.gguf"),
            ..Default::default()
        };
        cfg.normalize();
        assert_eq!(cfg.slm_escalation_gguf, dir.path().join("missing.gguf"));
        assert_eq!(cfg.effective_escalation_gguf(), primary.as_path());
        assert!(cfg.using_primary_for_escalation());
    }

    #[test]
    fn legacy_config_defaults_to_power_automate() {
        let cfg: Config = serde_json::from_str(
            r#"{"processing_dir":"P:/processing","outbox_dir":"P:/outbox","quarantine_dir":"P:/quarantine"}"#,
        )
        .unwrap();
        assert_eq!(cfg.output_mode, OutputMode::PowerAutomate);
        assert!(cfg.local_output_dir.as_os_str().is_empty());
        assert!(cfg.ready());
    }

    #[test]
    fn custom_naming_notes_default_is_empty() {
        assert_eq!(Config::default().custom_naming_notes, "");
    }

    #[test]
    fn custom_naming_notes_cap_accepts_600_and_rejects_601_chars() {
        let mut c = cfg("/a/proc", "/a/out", "/a/quar", "/a/cache");
        c.custom_naming_notes = "a".repeat(600);
        assert!(c.validate().is_ok());
        c.custom_naming_notes = "a".repeat(601);
        let error = c.validate().unwrap_err();
        assert!(error.contains("custom naming notes"), "{error}");
        // The cap applies to the trimmed text, so padding whitespace never
        // fails a value the operator sees as exactly 600 characters.
        c.custom_naming_notes = format!("  {}  ", "a".repeat(600));
        assert!(c.validate().is_ok());
    }

    #[test]
    fn normalize_strips_control_characters_from_naming_notes() {
        let mut c = Config {
            custom_naming_notes: "  keep\r\nlines\rtogether\u{7}\u{0} neat\t stuff  ".into(),
            ..Default::default()
        };
        c.normalize();
        // \r\n and bare \r became \n; the bell, NUL, and tab were dropped;
        // the padding was trimmed. No truncation happened.
        assert_eq!(c.custom_naming_notes, "keep\nlines\ntogether neat stuff");
    }

    #[test]
    fn config_json_without_naming_notes_loads_with_empty_default() {
        let mut cfg: Config = serde_json::from_str(
            r#"{"processing_dir":"P:/processing","outbox_dir":"P:/outbox","quarantine_dir":"P:/quarantine"}"#,
        )
        .unwrap();
        assert_eq!(cfg.custom_naming_notes, "");
        // And a set value survives a serialize/deserialize round trip.
        cfg.custom_naming_notes = "prefer client surnames".into();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.custom_naming_notes, "prefer client surnames");
    }

    #[test]
    fn local_mode_requires_local_output_and_rejects_outbox_overlap() {
        let mut cfg = Config {
            output_mode: OutputMode::Local,
            processing_dir: "/work/processing".into(),
            local_output_dir: "/work/output".into(),
            quarantine_dir: "/work/quarantine".into(),
            cache_dir: "/work/cache".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
        cfg.outbox_dir = "/work/output/_manifests".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn pinned_delivery_root_fails_closed_when_settings_make_it_protected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            processing_dir: dir.path().join("processing"),
            outbox_dir: dir.path().join("outbox"),
            local_output_dir: dir.path().join("local-output"),
            quarantine_dir: dir.path().join("quarantine"),
            cache_dir: dir.path().join("cache"),
            ..Default::default()
        };
        assert!(cfg
            .validate_pinned_delivery_root("local", &cfg.outbox_dir)
            .is_err());
        assert!(cfg
            .validate_pinned_delivery_root("power_automate", &cfg.local_output_dir)
            .is_err());
        assert!(cfg
            .validate_pinned_delivery_root("local", &cfg.local_output_dir)
            .is_ok());
        assert!(cfg
            .validate_pinned_delivery_root("power_automate", &cfg.outbox_dir)
            .is_ok());
    }
}
