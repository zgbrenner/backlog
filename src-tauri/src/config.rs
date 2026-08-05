//! BackLog configuration. Loaded from `backlog.config.json` next to the app
//! data dir; every field has a sane default so first launch works with only
//! the folder paths filled in from the UI.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

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
    /// Requests a primary llama-server serves before being killed and
    /// respawned — llama.cpp Windows RSS growth is unfixed upstream
    /// (ggml-org/llama.cpp#24356; measured 3.45->4.45 GB over 21 files).
    /// 0 disables recycling.
    pub slm_recycle_after_requests: u32,
    /// Seconds since the escalation server's last request COMPLETED (never
    /// mid-request — see `SlmLane::reap_idle_escalation`) before it is
    /// dropped. 0 disables idle-reaping (resident for the process lifetime).
    pub slm_escalation_idle_secs: u64,
    /// Max evidence tokens (approximate, chars/4) sent to the SLM.
    pub evidence_token_budget: usize,

    /// Optional fine-tuned Ettin token classifier directory (HF format).
    /// Empty string disables the Ettin lane gracefully.
    pub ettin_model_dir: String,

    /// How many `convertd` processes to run, and therefore how many documents
    /// can be converted at once. Each worker is a separate Python process at
    /// roughly 195 MB resident, so this is a memory knob as well as a
    /// throughput one — see `convert_workers_ram_ceiling`.
    pub convert_workers: usize,

    /// Maximum wait for one convertd request. A timed-out process is killed
    /// and lazily respawned on the next request.
    pub sidecar_timeout_secs: u64,

    /// Pace manifest emission (per minute, 0 = unlimited) to stay under
    /// Power Automate connector throttling on huge batches.
    pub manifest_emit_per_min: u32,

    /// Pages sampled for oversized documents.
    pub max_head_pages: usize,
    pub max_tail_pages: usize,

    /// Filename policy.
    pub max_filename_len: usize,

    /// Retry policy.
    pub max_stage_attempts: u8,
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
            // non-standard model license.
            slm_primary_gguf: PathBuf::from("models/Qwen3-0.6B-Q8_0.gguf"),
            slm_escalation_gguf: PathBuf::from("models/Qwen3-1.7B-Q8_0.gguf"),
            slm_parallel: default_slm_parallel(),
            slm_escalation_parallel: default_slm_escalation_parallel(),
            slm_recycle_after_requests: 64,
            slm_escalation_idle_secs: 600,
            evidence_token_budget: 1500,
            ettin_model_dir: String::new(),
            convert_workers: default_convert_workers(),
            sidecar_timeout_secs: 45,
            manifest_emit_per_min: 0,
            max_head_pages: 10,
            max_tail_pages: 3,
            max_filename_len: 120,
            max_stage_attempts: 3,
            per_file_wall_clock_secs: 90,
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

/// How many `convertd` workers installed RAM can hold.
///
/// This became a real constraint the moment `Sidecar` grew a process pool.
/// Before that, `convert_workers` only sized a semaphore and every request
/// funnelled through one child, so the value cost nothing in memory however
/// large it was. Now each worker is its own Python process — measured at
/// ~195 MB resident once MarkItDown and RapidOCR are loaded, plus a ~10 MB
/// PyInstaller bootstrap stub — so six of them is roughly 1.2 GB.
///
/// On an 8 GB machine that does not fit: Windows takes ~3 GB, the two model
/// servers take ~3.4 GB at `slm_parallel: 1`, and the app and WebView2 another
/// ~0.4 GB, which leaves about 1.4 GB. Two workers fit inside that with room to
/// spare; six do not, and the failure mode is the whole batch thrashing rather
/// than any single thing reporting an error.
///
/// A CPU-derived value below the ceiling still wins — this only caps.
fn convert_workers_ram_ceiling(gib: Option<u64>) -> usize {
    match gib {
        Some(g) if g <= 9 => 2,  // 8 GB class: ~400 MB of sidecars
        Some(g) if g <= 17 => 4, // 16 GB class
        Some(_) => 6,
        // Unknown RAM is not a reason to gamble on behalf of the smaller machine.
        None => 2,
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

/// How many naming slots to give llama-server, chosen from installed RAM.
///
/// This is a memory knob wearing a concurrency knob's name. `slm.rs` derives
/// `--ctx-size` as `4096 * slm_parallel`, and llama.cpp preallocates the whole
/// KV cache at startup, so the cost is linear and large: Qwen3 (28 layers,
/// 8 KV heads, head_dim 128, F16) needs 112 KiB per token, i.e. **448 MiB per
/// parallel slot**. Measured on Windows for Qwen3-0.6B: 590 MB private commit
/// at 1, 1,040 MB at 2, 1,938 MB at 4.
///
/// The escalation tier makes that worst case double. `SlmLane` keeps `primary`
/// and `escalation` in separate slots and the 1.7B server "remains resident for
/// the batch" once a third naming attempt wakes it, so a long run ends up
/// holding both. Measured together at the old flat default of 4: 6,078 MB of
/// working set for the two servers alone, which does not fit beside Windows,
/// the app and convertd on an 8 GB machine.
///
/// A flat 4 was therefore right for the workstation it was written on and
/// wrong for the laptops this ships to. Per-slot context is `4096` either way
/// (total is `4096 * n` across `n` slots), so lowering this costs no evidence
/// headroom — only cross-file naming overlap, which is rarely the bottleneck
/// because `Sidecar` serializes every conversion through one process anyway.
fn default_slm_parallel() -> u8 {
    slm_parallel_for_ram(total_ram_gib())
}

/// The decision itself, split from reading the machine so it can be tested.
///
/// The 8 GB branch is the entire reason this function exists and it is the one
/// branch the build machine can never exercise, so it is covered by
/// `slm_parallel_is_chosen_from_installed_ram` rather than by hoping.
fn slm_parallel_for_ram(gib: Option<u64>) -> u8 {
    match gib {
        Some(g) if g <= 9 => 1,  // 8 GB class: ~448 MiB of KV cache per server
        Some(g) if g <= 17 => 2, // 16 GB class
        Some(_) => 4,
        // Unknown RAM is not a reason to gamble on behalf of the smaller machine.
        None => 2,
    }
}

fn default_slm_escalation_parallel() -> u8 {
    slm_escalation_parallel_for_ram(total_ram_gib())
}

/// Deliberately never inherits `slm_parallel_for_ram`'s 4 — the whole point
/// of a separate knob is decoupling escalation's KV cost from primary's.
/// `docs/SIZING.md` measures 2,262 MB (parallel 1) vs 3,609 MB (parallel 4)
/// for the 1.7B; this keeps it at the low end always.
fn slm_escalation_parallel_for_ram(gib: Option<u64>) -> u8 {
    match gib {
        Some(g) if g <= 9 => 1,
        _ => 2, // >9 GiB and unknown RAM both get the small ceiling
    }
}

impl Config {
    pub fn load(path: &Path) -> Self {
        let parse = |candidate: &Path| -> Option<Self> {
            let contents = std::fs::read_to_string(candidate).ok()?;
            match serde_json::from_str(&contents) {
                Ok(cfg) => Some(cfg),
                Err(error) => {
                    log::warn!("config parse failed for {} ({error})", candidate.display());
                    None
                }
            }
        };
        let backup = backup_path(path);
        let (mut cfg, recovered_from_backup) = match parse(path) {
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
        if recovered_from_backup {
            let _ = std::fs::rename(&backup, path);
        }
        cfg.normalize();
        cfg.clamp_resources_to_machine();
        cfg
    }

    /// Lower `slm_parallel` to what installed RAM can hold, and say so.
    ///
    /// `default_slm_parallel` only decides what a *fresh* install writes, and
    /// `backlog.config.json` is persistent — so an 8 GB laptop upgrading from a
    /// build whose default was a flat 4 would keep 4 forever and thrash through
    /// its whole backfill, having never chosen that number. This is the upgrade
    /// path for that machine.
    ///
    /// Deliberately one-directional: a value at or below the RAM-derived
    /// ceiling is left exactly as configured, because someone lowering it knows
    /// something about their machine that this does not. Only an
    /// overcommitment is corrected, and never silently — at 4 on 8 GB the two
    /// model servers alone want ~6.1 GB of working set, which is not a slow
    /// run, it is a wedged one.
    #[cfg(test)]
    fn clamp_slm_parallel_for_test(&mut self, gib: Option<u64>) {
        let ceiling = slm_parallel_for_ram(gib);
        if self.slm_parallel > ceiling {
            self.slm_parallel = ceiling;
        }
    }

    /// Same one-directional contract as `clamp_slm_parallel_for_test`, for
    /// the escalation tier's own ceiling.
    #[cfg(test)]
    fn clamp_slm_escalation_parallel_for_test(&mut self, gib: Option<u64>) {
        let ceiling = slm_escalation_parallel_for_ram(gib);
        if self.slm_escalation_parallel > ceiling {
            self.slm_escalation_parallel = ceiling;
        }
    }

    #[cfg(test)]
    fn clamp_resources_for_test(&mut self, gib: Option<u64>) {
        self.slm_parallel = self.slm_parallel.min(slm_parallel_for_ram(gib));
        self.slm_escalation_parallel = self
            .slm_escalation_parallel
            .min(slm_escalation_parallel_for_ram(gib));
        self.convert_workers = self
            .convert_workers
            .min(convert_workers_ram_ceiling(gib))
            .max(1);
    }

    /// Apply the machine's memory ceilings to loaded and newly submitted
    /// settings. This is intentionally one-directional: conservative custom
    /// values survive, while an old or imported high-memory preset is made
    /// safe before it can start worker processes.
    pub fn clamp_resources_to_machine(&mut self) {
        let gib = total_ram_gib();
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
                "convert_workers {} exceeds this machine's safe memory budget; using {}",
                self.convert_workers,
                convert_ceiling
            );
            self.convert_workers = convert_ceiling;
        }
        self.convert_workers = self.convert_workers.max(1);
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
        if self.evidence_token_budget == 0 || self.evidence_token_budget > 16_384 {
            return Err(format!(
                "evidence_token_budget must be between 1 and 16384; got {}.",
                self.evidence_token_budget
            ));
        }
        if self.convert_workers == 0 || self.convert_workers > 8 {
            return Err(format!(
                "convert_workers must be between 1 and 8; got {}.",
                self.convert_workers
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
        c.convert_workers = 0;
        cases.push((c, "convert_workers"));

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
    /// `4096 * slm_parallel` and llama.cpp preallocates the whole KV cache, at
    /// 448 MiB per slot per server — doubled once the escalation tier wakes and
    /// stays resident. Measured, both servers at 4: 6,078 MB of working set,
    /// which does not fit on an 8 GB machine beside Windows and convertd.
    ///
    /// The 8 GB row is the reason this logic exists and is the one row a
    /// 62 GB build machine can never produce, so it is asserted here.
    #[test]
    fn slm_parallel_is_chosen_from_installed_ram() {
        for (gib, expected) in [
            (4u64, 1u8),
            (8, 1),
            (9, 1),
            (12, 2),
            (16, 2),
            (17, 2),
            (32, 4),
        ] {
            assert_eq!(
                slm_parallel_for_ram(Some(gib)),
                expected,
                "{gib} GiB should give {expected}"
            );
        }
        // Unknown RAM must not gamble on behalf of the smaller machine.
        assert_eq!(slm_parallel_for_ram(None), 2);
    }

    /// The escalation tier deliberately never inherits `slm_parallel_for_ram`'s
    /// 4 — it stays at the low end (1 or 2) on every machine, because its
    /// whole point is decoupling escalation's KV cost from primary's.
    #[test]
    fn slm_escalation_parallel_is_chosen_from_installed_ram() {
        for (gib, expected) in [
            (4u64, 1u8),
            (8, 1),
            (9, 1),
            (12, 2),
            (16, 2),
            (17, 2),
            (32, 2),
        ] {
            assert_eq!(
                slm_escalation_parallel_for_ram(Some(gib)),
                expected,
                "{gib} GiB should give {expected}"
            );
        }
        // Unknown RAM must not gamble on behalf of the smaller machine.
        assert_eq!(slm_escalation_parallel_for_ram(None), 2);
    }

    /// Each `convertd` worker is a ~195 MB Python process now that `Sidecar`
    /// pools them, so six of them is ~1.2 GB and does not fit on 8 GB beside
    /// Windows, the two model servers and the app.
    #[test]
    fn convert_workers_are_capped_by_installed_ram() {
        for (gib, expected) in [(4u64, 2usize), (8, 2), (9, 2), (12, 4), (17, 4), (32, 6)] {
            assert_eq!(
                convert_workers_ram_ceiling(Some(gib)),
                expected,
                "{gib} GiB should cap at {expected}"
            );
        }
        assert_eq!(convert_workers_ram_ceiling(None), 2);
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
        cfg.clamp_slm_parallel_for_test(Some(8));
        assert_eq!(cfg.slm_parallel, 1, "8 GB must not inherit 4");

        // One-directional: someone who lowered it knows their machine.
        let mut cfg = Config {
            slm_parallel: 1,
            ..Default::default()
        };
        cfg.clamp_slm_parallel_for_test(Some(64));
        assert_eq!(cfg.slm_parallel, 1, "a deliberate 1 must survive on 64 GB");
    }

    /// Same one-directional contract for the escalation tier's own knob.
    #[test]
    fn a_persisted_slm_escalation_parallel_is_clamped_down_but_never_up() {
        let mut cfg = Config {
            slm_escalation_parallel: 2,
            ..Default::default()
        };
        cfg.clamp_slm_escalation_parallel_for_test(Some(8));
        assert_eq!(cfg.slm_escalation_parallel, 1, "8 GB must not inherit 2");

        // One-directional: someone who lowered it knows their machine.
        let mut cfg = Config {
            slm_escalation_parallel: 1,
            ..Default::default()
        };
        cfg.clamp_slm_escalation_parallel_for_test(Some(64));
        assert_eq!(
            cfg.slm_escalation_parallel, 1,
            "a deliberate 1 must survive on 64 GB"
        );
    }

    #[test]
    fn an_eight_gib_machine_clamps_every_process_pool() {
        let mut cfg = Config {
            slm_parallel: 4,
            slm_escalation_parallel: 2,
            convert_workers: 6,
            ..Default::default()
        };
        cfg.clamp_resources_for_test(Some(8));
        assert_eq!(cfg.slm_parallel, 1);
        assert_eq!(cfg.slm_escalation_parallel, 1);
        assert_eq!(cfg.convert_workers, 2);
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
