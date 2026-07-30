//! BackLog configuration. Loaded from `backlog.config.json` next to the app
//! data dir; every field has a sane default so first launch works with only
//! the folder paths filled in from the UI.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// OneDrive-synced folder Power Automate Flow 1 moves intake files into.
    pub processing_dir: PathBuf,
    /// OneDrive-synced folder the app writes per-file manifests into
    /// (Flow 2 triggers on `<outbox_dir>/_manifests`).
    pub outbox_dir: PathBuf,
    /// Local quarantine for flagged files (not synced).
    pub quarantine_dir: PathBuf,
    /// Local cache: converted markdown + evidence bundles, keyed by sha256.
    pub cache_dir: PathBuf,

    /// llama-server settings.
    pub llama_port: u16,
    pub slm_primary_gguf: PathBuf,
    pub slm_escalation_gguf: PathBuf,
    pub slm_parallel: u8,
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
            processing_dir: PathBuf::new(),
            outbox_dir: PathBuf::new(),
            quarantine_dir: PathBuf::new(),
            cache_dir: PathBuf::new(),
            llama_port: 8137,
            // Apache-2.0 Qwen3 GGUFs (llama.cpp) replace the Liquid-licensed
            // LFM2.5 pair so the app can be redistributed without a
            // non-standard model license.
            slm_primary_gguf: PathBuf::from("models/Qwen3-0.6B-Q8_0.gguf"),
            slm_escalation_gguf: PathBuf::from("models/Qwen3-1.7B-Q8_0.gguf"),
            slm_parallel: default_slm_parallel(),
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

impl Config {
    pub fn load(path: &Path) -> Self {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                log::warn!("config parse failed ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        };
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

    #[cfg(test)]
    fn clamp_resources_for_test(&mut self, gib: Option<u64>) {
        self.slm_parallel = self.slm_parallel.min(slm_parallel_for_ram(gib));
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
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.outbox_dir.join("_manifests")
    }

    pub fn ready(&self) -> bool {
        !self.processing_dir.as_os_str().is_empty()
            && !self.outbox_dir.as_os_str().is_empty()
            && !self.quarantine_dir.as_os_str().is_empty()
    }

    /// Reject configurations that would corrupt processing: unset folders,
    /// duplicate folders, or folders nested inside one another. The watcher is
    /// recursive over the processing dir, so a nested outbox/cache/quarantine
    /// would feed the app's own manifests and cached markdown back into the
    /// pipeline as if they were intake documents.
    pub fn validate(&self) -> Result<(), String> {
        if !self.ready() {
            return Err("Set the Processing, Outbox, and Quarantine folders first.".into());
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
        let named: [(&str, &Path); 4] = [
            ("Processing", self.processing_dir.as_path()),
            ("Outbox", self.outbox_dir.as_path()),
            ("Quarantine", self.quarantine_dir.as_path()),
            ("Cache", self.cache_dir.as_path()),
        ];
        for i in 0..named.len() {
            let (a_name, a_path) = named[i];
            if a_path.as_os_str().is_empty() {
                continue;
            }
            let a = lexical_norm(a_path);
            for (b_name, b_path) in named.iter().skip(i + 1) {
                if b_path.as_os_str().is_empty() {
                    continue;
                }
                let b = lexical_norm(b_path);
                if a == b {
                    return Err(format!("{a_name} and {b_name} folders must be different."));
                }
                if a.starts_with(&b) || b.starts_with(&a) {
                    return Err(format!(
                        "{a_name} and {b_name} folders must not be nested inside each other."
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

    #[test]
    fn an_eight_gib_machine_clamps_every_process_pool() {
        let mut cfg = Config {
            slm_parallel: 4,
            convert_workers: 6,
            ..Default::default()
        };
        cfg.clamp_resources_for_test(Some(8));
        assert_eq!(cfg.slm_parallel, 1);
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
}
