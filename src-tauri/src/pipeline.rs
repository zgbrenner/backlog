//! The orchestrator. Owns worker pools, drives each file through the state
//! machine, implements the §7 retry ladder (retries vary the input; identical
//! retries are prayer), and quarantines with machine-readable reasons.

use crate::checker::{fs_metadata_dates, CheckError, Checker};
use crate::config::Config;
use crate::filter::{self, Evidence};
use crate::ledger::{JobState, Ledger};
use crate::manifest::{write_manifest, Manifest, Pacer, MANIFEST_SCHEMA_VERSION};
use crate::routing::{self, Route};
use crate::sidecar::{ConvertResult, Sidecar};
use crate::slm::{SlmLane, Tier};
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::Emitter;
use tokio::sync::Semaphore;

pub struct Pipeline {
    pub cfg: Config,
    pub ledger: Arc<Ledger>,
    pub sidecar: Arc<Sidecar>,
    pub slm: Arc<SlmLane>,
    /// `None` only in this module's tests, which drive the orchestrator
    /// headlessly; there is no way to build a real `AppHandle` without a
    /// running Tauri app, and the UI notification is the one thing the
    /// pipeline does that has no bearing on file safety.
    pub app: Option<tauri::AppHandle>,
    pub paused: Arc<AtomicBool>,
    convert_slots: Arc<Semaphore>,
    slm_slots: Arc<Semaphore>,
    /// Global cap on files being hashed / stability-probed / processed at once,
    /// so a large backfill applies backpressure instead of spawning thousands
    /// of concurrent blocking probes.
    pub ingest_slots: Arc<Semaphore>,
    /// Paths currently being stability-probed, hashed or processed. The ledger
    /// claim is the durable guard, but it costs a full SHA-256 of the file to
    /// reach; this makes the second enqueue of the same path free.
    inflight: Arc<Mutex<HashSet<PathBuf>>>,
    pacer: Arc<Pacer>,
    model_versions: serde_json::Value,
}

const PDF_TEXT_MEDIAN_CHARS: u64 = 200;
const OCR_CONF_FLOOR: f64 = 0.55;

/// A job may be claimed for this many multiples of the per-file wall-clock cap
/// before the claim is treated as abandoned. Anything shorter risks two live
/// workers on one file; anything longer strands a file whose owner crashed.
const CLAIM_STALE_MULTIPLE: u64 = 6;

/// The per-file wall-clock cap, in seconds of WORK — waiting for a convert
/// slot, an SLM slot or an emit permit does not count (see [`WorkClock`]).
///
/// It has to sit above the sum of the stage timeouts it wraps, or it fires on
/// perfectly healthy documents. The bare `per_file_wall_clock_secs * 3` it
/// replaces was 270s by default, while the convert stage alone is permitted
/// `sidecar_timeout_secs * max_stage_attempts` = 135s before the pdf probe,
/// build_evidence's four sidecar round-trips and the naming ladder have run at
/// all. That miscalibration used to cost one log line; now that the cap
/// quarantines the file, ships a `flagged` manifest to SharePoint and freezes
/// the row, it costs a document.
///
/// `per_file_wall_clock_secs` stands in for one naming request because the
/// naming lane has no timeout knob of its own; its default (90s) is already
/// above `SlmLane`'s 60s HTTP timeout, so the substitution is conservative and
/// the knob keeps its meaning — raising it raises the backstop.
fn wall_clock_cap(cfg: &Config) -> u64 {
    let attempts = (cfg.max_stage_attempts.max(1)) as u64;
    // pdf_probe (1) + one convert/OCR attempt per rung + build_evidence's
    // langid, classify, salience and ettin round-trips (4).
    let sidecar = cfg.sidecar_timeout_secs.saturating_mul(attempts + 5);
    // One naming request per rung, plus the span-mismatch re-prompt.
    let naming = cfg.per_file_wall_clock_secs.saturating_mul(attempts + 1);
    sidecar.saturating_add(naming).max(1)
}

/// Queue time already served, plus the wait in progress.
#[derive(Default)]
struct Queued {
    total: std::time::Duration,
    /// `Some` while the file is parked on backpressure right now. Without it a
    /// single wait longer than the cap would blow the deadline before it ever
    /// got the chance to credit itself.
    since: Option<std::time::Instant>,
}

/// State shared between `process_file` and the `process_inner` future it may
/// have to drop mid-flight: which ledger row this worker owns, and how much of
/// the elapsed time was spent queueing rather than working.
///
/// Queue time is not the file's own. `convert_slots`, `slm_slots` and the emit
/// `Pacer` are backpressure by design — the defaults on a 4-core box admit 8
/// files concurrently against 2 convert slots, and `manifest_emit_per_min` is
/// an operator knob documented for exactly this backfill. Counting a wait at
/// any of them against the wall-clock cap quarantines healthy documents for
/// being queued behind other healthy documents, which is precisely the shape of
/// failure a multi-thousand-file backfill produces.
#[derive(Default)]
struct WorkClock {
    sha: Mutex<Option<String>>,
    queued: Mutex<Queued>,
}

impl WorkClock {
    /// Run `f` off the clock: whatever it spends waiting is added back to the
    /// deadline `process_file` is holding.
    async fn parked<F: std::future::Future>(&self, f: F) -> F::Output {
        self.queued.lock().unwrap().since = Some(std::time::Instant::now());
        let out = f.await;
        let mut q = self.queued.lock().unwrap();
        if let Some(since) = q.since.take() {
            q.total += since.elapsed();
        }
        out
    }

    fn queued_total(&self) -> std::time::Duration {
        let q = self.queued.lock().unwrap();
        q.total + q.since.map(|s| s.elapsed()).unwrap_or_default()
    }

    fn owned_sha(&self) -> Option<String> {
        self.sha.lock().unwrap().clone()
    }
}

/// Releases the ledger claim however the job ends — normal return, early flag,
/// or the wall-clock timeout dropping the whole future out from under it.
/// Without the Drop impl every timeout would leave a claim nobody releases
/// until it goes stale.
struct ClaimGuard {
    ledger: Arc<Ledger>,
    sha: String,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if let Err(e) = self.ledger.release_claim(&self.sha) {
            log::warn!("could not release claim on {}: {e}", self.sha);
        }
    }
}

/// Holds a path in `Pipeline::inflight` for the lifetime of one enqueue.
pub struct InFlightPath {
    set: Arc<Mutex<HashSet<PathBuf>>>,
    path: PathBuf,
}

impl Drop for InFlightPath {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.path);
    }
}

impl Pipeline {
    pub fn new(
        cfg: Config,
        ledger: Arc<Ledger>,
        sidecar: Arc<Sidecar>,
        slm: Arc<SlmLane>,
        app: tauri::AppHandle,
    ) -> Arc<Self> {
        let model_versions = sidecar.versions().unwrap_or_else(|_| json!({}));
        Arc::new(Self {
            convert_slots: Arc::new(Semaphore::new(cfg.convert_workers.max(1))),
            slm_slots: Arc::new(Semaphore::new(cfg.slm_parallel.max(1) as usize)),
            ingest_slots: Arc::new(Semaphore::new(
                (cfg.convert_workers.max(1) * 4).clamp(8, 64),
            )),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            pacer: Arc::new(Pacer::new(cfg.manifest_emit_per_min)),
            cfg,
            ledger,
            sidecar,
            slm,
            app: Some(app),
            paused: Arc::new(AtomicBool::new(false)),
            model_versions,
        })
    }

    fn emit_update(&self, sha: &str) {
        let Some(app) = &self.app else { return };
        if let Ok(Some(job)) = self.ledger.get(sha) {
            let _ = app.emit("job-updated", &job);
        }
    }

    /// Reserve `path` for one enqueue. `None` means another task already holds
    /// it — the caller must drop the event rather than re-hash the file.
    pub fn begin_path(&self, path: &Path) -> Option<InFlightPath> {
        let mut set = self.inflight.lock().unwrap();
        if !set.insert(path.to_path_buf()) {
            return None;
        }
        Some(InFlightPath {
            set: self.inflight.clone(),
            path: path.to_path_buf(),
        })
    }

    /// Advance the state machine, or give up on the job. `Ok(false)` from the
    /// guarded CAS means this worker no longer owns the row (it was flagged,
    /// emitted, or claimed by someone else), and the only safe response is to
    /// stop touching the file.
    fn advance(&self, sha: &str, state: JobState) -> bool {
        match self.ledger.set_state(sha, state) {
            Ok(true) => true,
            Ok(false) => {
                log::info!(
                    "abandoning {sha}: transition to {} refused; another worker owns this job",
                    state.as_str()
                );
                false
            }
            Err(e) => {
                log::error!("state update failed for {sha}: {e}");
                false
            }
        }
    }

    /// Entry point per discovered file. Spawned as a task; bounded by pools.
    pub async fn process_file(self: Arc<Self>, path: PathBuf) {
        // Hold while paused rather than dropping the file — dropping would
        // strand anything that arrived during a pause until the next restart's
        // sweep re-discovered it.
        while self.paused.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        // The timeout drops `process_inner` mid-flight, so the sha it computed
        // would be lost with it. Hoist it out through a shared slot, set the
        // moment this worker actually owns the row, so the handler below knows
        // which job to quarantine.
        let clock = Arc::new(WorkClock::default());
        let cap = std::time::Duration::from_secs(wall_clock_cap(&self.cfg));
        let started = tokio::time::Instant::now();
        let mut inner = Box::pin(self.clone().process_inner(path.clone(), clock.clone()));
        // A plain `tokio::time::timeout` cannot express "except while queued":
        // the deadline moves forward as the file sits on backpressure, so the
        // sleep is re-armed against the current deadline instead of being fixed
        // at spawn time. Each extension costs one extra wakeup, not a poll loop.
        let timed_out = loop {
            let deadline = started + cap + clock.queued_total();
            tokio::select! {
                _ = &mut inner => break false,
                _ = tokio::time::sleep_until(deadline) => {
                    if tokio::time::Instant::now() >= started + cap + clock.queued_total() {
                        break true;
                    }
                }
            }
        };
        if timed_out {
            match clock.owned_sha() {
                // Abandoning the job here is what produced "stuck at converted
                // for 40 minutes" rows the UI could not tell from "converting
                // right now", and a restart sweep that eventually quarantined
                // the file as CRASH_LOOP — a diagnosis that was simply false.
                Some(sha) => {
                    let stage = self.stage_of(&sha);
                    let secs = cap.as_secs();
                    log::error!("wall-clock cap blown for {path:?} at stage {stage}");
                    self.flag(
                        &sha,
                        &path,
                        format!("TIMEOUT:exceeded {secs}s at stage {stage}"),
                        &clock,
                    )
                    .await;
                }
                None => log::error!("wall-clock cap blown for {path:?} before it was identified"),
            }
        }
    }

    /// Where the file actually is. `active_stage` is stamped when a stage
    /// begins; `last_stage` names the last one that finished and is the answer
    /// only before any stage has started.
    fn stage_of(&self, sha: &str) -> String {
        self.ledger
            .get(sha)
            .ok()
            .flatten()
            .and_then(|j| j.active_stage.or(j.last_stage))
            .unwrap_or_else(|| "ingest".into())
    }

    async fn process_inner(self: Arc<Self>, path: PathBuf, clock: Arc<WorkClock>) {
        // ---- Ingest --------------------------------------------------------
        // Streams the whole file through SHA-256; off the async runtime so a
        // large file can't starve other in-flight jobs' wakeups. A JoinError
        // (blocking task panicked) is folded into the same anyhow::Result as
        // a normal hash failure, so the existing error handling below covers
        // both without unwrap-panicking the worker.
        let hash_path = path.clone();
        let hashed: anyhow::Result<String> =
            match tokio::task::spawn_blocking(move || hash_file(&hash_path)).await {
                Ok(r) => r,
                Err(join_err) => Err(anyhow::anyhow!("hash task panicked: {join_err}")),
            };
        let sha = match hashed {
            Ok(h) => h,
            Err(e) => {
                log::warn!("hash failed for {path:?}: {e} (sync race? will retry on next event)");
                return;
            }
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let ext = routing::extension_of(&path);
        let original_relpath = relpath(&self.cfg.processing_dir, &path);

        let resume_state = match self.ledger.ingest(
            &sha,
            &path.to_string_lossy(),
            &name,
            &original_relpath,
            &ext,
        ) {
            Ok(None) => JobState::Ingested, // new
            Ok(Some(existing)) => {
                if existing.state.is_resolved() || existing.state == JobState::Flagged {
                    // Same content seen again under a *different* file: emit a
                    // duplicate manifest so PA can index " (2)". Compare the
                    // normalized Processing-relative paths, which is what
                    // identity means here — the old raw-absolute-string test
                    // reclassified every job the moment the Processing folder
                    // moved, and fired on P4's restore path every time.
                    let known = existing.original_relpath.clone().unwrap_or_else(|| {
                        relpath(&self.cfg.processing_dir, Path::new(&existing.original_path))
                    });
                    if crate::identity::normalize_relpath(&known)
                        != crate::identity::normalize_relpath(&original_relpath)
                    {
                        self.handle_duplicate(&sha, &path, &name, &ext, &existing, &clock)
                            .await;
                    }
                    return;
                }
                existing.state // resume mid-flight job below
            }
            Err(e) => {
                log::error!("ledger ingest failed: {e}");
                return;
            }
        };

        // Single atomic gate: the startup sweep and a watcher event both reach
        // this line for the same file, and only one may proceed. Two workers
        // meant two proposals, and llama.cpp with --parallel is not bitwise
        // deterministic — when they differed the loser quarantined a document
        // whose valid `ok` manifest was already in the Outbox.
        let _claim = match self
            .ledger
            .try_claim(&sha, wall_clock_cap(&self.cfg) * CLAIM_STALE_MULTIPLE)
        {
            Ok(true) => ClaimGuard {
                ledger: self.ledger.clone(),
                sha: sha.clone(),
            },
            Ok(false) => {
                log::info!("{sha} is already claimed by another worker; dropping this enqueue");
                return;
            }
            Err(e) => {
                log::error!("claim failed for {sha}: {e}");
                return;
            }
        };
        *clock.sha.lock().unwrap() = Some(sha.clone());

        // The sha256 is the event's key and original_path/name live in the
        // jobs row; don't duplicate the (PII) path into the audit log.
        let _ = self.ledger.log_event(&sha, "ingest", "ingested");
        self.emit_update(&sha);

        // Crash-loop guard. Counts claims taken *without leaving a stage*, not
        // enqueues: the old counter was incremented by every duplicate event
        // and every restart re-pickup, so a healthy document that was merely
        // re-enqueued got force-quarantined with a reason telling a
        // non-technical operator it had crashed the daemon.
        const CRASH_LOOP_LIMIT: i64 = 5;
        let stage = resume_state.as_str();
        let attempts = self.ledger.bump_stage_attempts(&sha, stage).unwrap_or(0);
        if attempts > CRASH_LOOP_LIMIT {
            self.flag(
                &sha,
                &path,
                format!("CRASH_LOOP:{attempts} attempts without leaving stage {stage}"),
                &clock,
            )
            .await;
            return;
        }

        // ---- Route ---------------------------------------------------------
        // Reads header bytes + infers magic bytes; blocking file I/O, off the
        // async runtime. sha is already known here, so a JoinError routes to
        // flag/quarantine like any other routing failure would.
        //
        // Every blocking section announces itself before it blocks, so the
        // wall-clock timeout can name where the file actually is. `last_stage`
        // alone is written only *after* a stage succeeds, so a file wedged in
        // OCR reported "at stage ingested".
        let _ = self.ledger.mark_stage(&sha, "route");
        let route_path = path.clone();
        let decision = match tokio::task::spawn_blocking(move || routing::detect(&route_path)).await
        {
            Ok(d) => d,
            Err(join_err) => {
                self.flag(
                    &sha,
                    &path,
                    format!("RUNTIME_FAIL:routing task panicked: {join_err}"),
                    &clock,
                )
                .await;
                return;
            }
        };
        let _ = self.ledger.update_fields(
            &sha,
            &[("detected_type", Some(decision.detected_type.clone()))],
        );
        if decision.route == Route::Flag {
            self.flag(
                &sha,
                &path,
                decision.flag_reason.unwrap_or_else(|| "UNSUPPORTED".into()),
                &clock,
            )
            .await;
            return;
        }

        // PDF text-layer probe decides native vs scanned.
        let mut route = decision.route;
        if decision.detected_type == "application/pdf" {
            // Sidecar round-trip is blocking (stdin/stdout over a pipe with a
            // recv_timeout inside); off the async runtime. A JoinError is
            // folded into the anyhow::Result so it falls through the same
            // "transient? one implicit retry via convert below" path as any
            // other non-password pdf_probe error, unchanged from before.
            let sidecar = self.sidecar.clone();
            let p = path.to_string_lossy().to_string();
            let probe: anyhow::Result<(u64, u64)> =
                match tokio::task::spawn_blocking(move || sidecar.pdf_probe(&p)).await {
                    Ok(r) => r,
                    Err(join_err) => Err(anyhow::anyhow!("pdf_probe task panicked: {join_err}")),
                };
            match probe {
                Ok((median, _pages)) => {
                    if median < PDF_TEXT_MEDIAN_CHARS {
                        route = Route::Scanned;
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("password") || msg.contains("encrypted") {
                        // Retrying can't fix a password. No retry.
                        self.flag(&sha, &path, "ENCRYPTED:password protected".into(), &clock)
                            .await;
                        return;
                    }
                    // transient? one implicit retry happens via convert below
                }
            }
        }
        let _ = self.ledger.update_fields(
            &sha,
            &[("route", Some(format!("{route:?}").to_lowercase()))],
        );

        // ---- Convert (retry ladder row 1-2) --------------------------------
        let _ = self.ledger.mark_stage(&sha, "convert");
        let conv = {
            // Queueing for a convert slot is backpressure, not this file's
            // work: `parked` credits the wait back to the deadline.
            let _permit = clock.parked(self.convert_slots.acquire()).await.unwrap();
            self.convert_with_retries(&sha, &path, route, &clock).await
        };
        let conv = match conv {
            Some(c) => c,
            None => return, // already flagged inside
        };
        if conv.encrypted {
            self.flag(&sha, &path, "ENCRYPTED:password protected".into(), &clock)
                .await;
            return;
        }
        if conv.markdown.trim().len() < 30 {
            self.flag(&sha, &path, "CONVERT_FAIL:empty extraction".into(), &clock)
                .await;
            return;
        }
        // Multi-doc packet heuristic: several letterhead/date resets.
        let mut extra_soft: Vec<String> = Vec::new();
        if conv.letterhead_resets >= 2 {
            extra_soft.push("POSSIBLE_MULTIDOC".into());
        }
        if !self.advance(&sha, JobState::Converted) {
            return;
        }
        self.emit_update(&sha);

        // Cache markdown for the review pane and Ettin training.
        let cache = self.cfg.cache_dir.join(format!("{sha}.md"));
        let _ = std::fs::create_dir_all(&self.cfg.cache_dir);
        let _ = std::fs::write(&cache, &conv.markdown);

        // ---- Filter --------------------------------------------------------
        // build_evidence itself makes several blocking sidecar round-trips
        // (langid/classify/salience/ettin); run the whole thing off the async
        // runtime. Clone the Arc<Sidecar> + an owned copy of the markdown and
        // doc_meta_dates into the closure; a JoinError folds into the same
        // anyhow::Result the fallible call already returns, so it flags like
        // any other filter failure.
        let _ = self.ledger.mark_stage(&sha, "filter");
        let ettin_enabled = !self.cfg.ettin_model_dir.is_empty();
        let sidecar = self.sidecar.clone();
        let markdown = conv.markdown.clone();
        let doc_meta_dates = conv.doc_meta_dates.clone();
        let token_budget = self.cfg.evidence_token_budget;
        let filtered: anyhow::Result<filter::FilterOutcome> =
            match tokio::task::spawn_blocking(move || {
                filter::build_evidence(
                    &sidecar,
                    &markdown,
                    doc_meta_dates,
                    ettin_enabled,
                    token_budget,
                )
            })
            .await
            {
                Ok(r) => r,
                Err(join_err) => Err(anyhow::anyhow!("build_evidence task panicked: {join_err}")),
            };
        let filtered = match filtered {
            Ok(f) => f,
            Err(e) => {
                self.flag(&sha, &path, format!("RUNTIME_FAIL:filter {e}"), &clock)
                    .await;
                return;
            }
        };
        let ev = filtered.evidence;
        // doc_type stays NULL when no classifier ran: the torch-free profile's
        // fallback label is a constant, and writing it would put a fabricated
        // classification into every row of the SharePoint DocumentIndex.
        let _ = self.ledger.update_fields(
            &sha,
            &[
                ("doc_type", ev.doc_type.clone()),
                ("language", Some(ev.language.clone())),
            ],
        );
        if !self.advance(&sha, JobState::Filtered) {
            return;
        }
        self.emit_update(&sha);

        // ---- Name + Validate (retry ladder rows 3-5) -----------------------
        let (fs_dates, modified_iso) = fs_metadata_dates(&path);
        let mut meta_dates = filtered.doc_meta_dates;
        meta_dates.extend(fs_dates);
        meta_dates.dedup();

        let checker = Checker::new(self.cfg.max_filename_len);
        let ettin_date: Option<String> = ev
            .ettin_spans
            .iter()
            .filter(|s| s.label == "DATE")
            .max_by(|a, b| a.score.total_cmp(&b.score))
            .and_then(|s| s.iso.clone());

        let _ = self.ledger.mark_stage(&sha, "name");
        let validated = {
            let _permit = clock.parked(self.slm_slots.acquire()).await.unwrap();
            self.name_with_retries(
                &sha,
                &ev,
                &checker,
                &meta_dates,
                &modified_iso,
                ettin_date.as_deref(),
            )
            .await
        };
        let mut validated = match validated {
            Some(v) => v,
            None => {
                self.flag(
                    &sha,
                    &path,
                    "SLM_FAIL:no valid output after escalation".into(),
                    &clock,
                )
                .await;
                return;
            }
        };
        validated.soft_flags.extend(extra_soft);
        if !self.advance(&sha, JobState::Validated) {
            return;
        }

        // ---- Compose final name, reserve, emit ------------------------------
        // The reservation IS the write: probing for a free name and then
        // writing it in a separate statement let two same-day invoices from
        // one vendor both book "2024-03-05 Acme Invoice.pdf".
        let _ = self.ledger.mark_stage(&sha, "emit");
        let final_filename = match self.ledger.reserve_name(&validated.base_name, &ext, &sha) {
            Ok(n) => n,
            Err(e) => {
                self.flag(
                    &sha,
                    &path,
                    format!("RUNTIME_FAIL:name reservation {e}"),
                    &clock,
                )
                .await;
                return;
            }
        };
        let _ = self.ledger.update_fields(
            &sha,
            &[
                ("proposed_date", Some(validated.date_iso.clone())),
                ("date_source", Some(validated.date_source.clone())),
                ("proposed_subject", Some(validated.subject.clone())),
                ("description", Some(validated.description.clone())),
                ("final_filename", Some(final_filename.clone())),
                ("soft_flags", Some(validated.soft_flags.join(","))),
                ("model_versions", Some(self.model_versions.to_string())),
            ],
        );

        // `manifest_emit_per_min` exists to park emissions on purpose during a
        // backfill; a paced file must not be quarantined for obeying the pace.
        clock.parked(self.pacer.permit()).await;
        let m = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: manifest_id(&sha, &original_relpath),
            sha256: sha.clone(),
            status: "ok".into(),
            original_name: name,
            original_relpath,
            new_filename: Some(final_filename),
            description: Some(validated.description),
            date: Some(validated.date_iso),
            date_source: Some(validated.date_source),
            doc_type: ev.doc_type,
            language: Some(ev.language),
            duplicate_of: None,
            soft_flags: validated.soft_flags,
            flag_reason: None,
            model_versions: self.model_versions.clone(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        match write_manifest(&self.cfg.manifests_dir(), &m) {
            Ok(_) => {
                let _ = self.advance(&sha, JobState::Emitted);
                let _ = self.ledger.log_event(&sha, "emit", "manifest written");
                // File is done; drop its raw document text from the cache.
                self.purge_cache(&sha);
            }
            Err(e) => {
                self.flag(&sha, &path, format!("RUNTIME_FAIL:manifest {e}"), &clock)
                    .await;
                return;
            }
        }
        self.emit_update(&sha);
    }

    async fn convert_with_retries(
        &self,
        sha: &str,
        path: &Path,
        route: Route,
        clock: &WorkClock,
    ) -> Option<ConvertResult> {
        let p = path.to_string_lossy().to_string();
        let (hp, tp) = (self.cfg.max_head_pages, self.cfg.max_tail_pages);
        for attempt in 1..=self.cfg.max_stage_attempts {
            // Clone the Arc<Sidecar> + an owned copy of the path per attempt
            // so the blocking convert/OCR round-trip runs off the async
            // runtime; convert_slots stays acquired around this whole loop in
            // the async caller (process_inner), so the permit isn't held
            // inside the blocking closure. A JoinError folds into the same
            // anyhow::Result a normal convert/OCR failure returns, so it
            // flows through the existing retry-then-flag logic below
            // unchanged.
            let sidecar = self.sidecar.clone();
            let p2 = p.clone();
            let result: anyhow::Result<ConvertResult> =
                match tokio::task::spawn_blocking(move || {
                    match (route, attempt) {
                        // Native path: MarkItDown; attempt 3 falls through to raw
                        // pdfium text dump / OCR inside the sidecar's fallback op.
                        (Route::Native, 1 | 2) => sidecar.convert(&p2, hp, tp),
                        (Route::Native, _) => sidecar.ocr(&p2, 300, hp, tp),
                        // Scanned path: 300 DPI, then 400 DPI, then an enhanced
                        // 600 DPI + grayscale/autocontrast classical OCR pass (the
                        // sidecar selects it via the dpi=0 sentinel).
                        (Route::Scanned, 1) => sidecar.ocr(&p2, 300, hp, tp),
                        (Route::Scanned, 2) => sidecar.ocr(&p2, 400, hp, tp),
                        (Route::Scanned, _) => sidecar.ocr(&p2, 0, hp, tp), // 0 = enhanced OCR
                        (Route::Flag, _) => unreachable!(),
                    }
                })
                .await
                {
                    Ok(r) => r,
                    Err(join_err) => Err(anyhow::anyhow!("convert/ocr task panicked: {join_err}")),
                };
            match result {
                Ok(c) => {
                    if c.ocr_used
                        && c.ocr_mean_conf < OCR_CONF_FLOOR
                        && attempt < self.cfg.max_stage_attempts
                    {
                        let _ = self.ledger.log_event(
                            sha,
                            "convert",
                            &format!(
                                "attempt {attempt}: ocr conf {:.2} below floor, escalating",
                                c.ocr_mean_conf
                            ),
                        );
                        continue;
                    }
                    return Some(c);
                }
                Err(e) => {
                    // Sidecar errors embed the document's absolute path; the
                    // ledger gets the stable code, the app log gets the detail.
                    log::warn!("convert attempt {attempt} failed: {e}");
                    let _ = self.ledger.log_event(
                        sha,
                        "convert",
                        &format!("attempt {attempt} failed: {}", error_code(&e)),
                    );
                }
            }
        }
        let reason = if route == Route::Scanned {
            "UNREADABLE"
        } else {
            "CONVERT_FAIL"
        };
        self.flag(
            sha,
            path,
            format!("{reason}:all conversion attempts exhausted"),
            clock,
        )
        .await;
        None
    }

    /// Which model tier and which evidence bundle rung `attempt` uses.
    ///
    /// Split out from the loop because this — not the model swap — is the
    /// ladder's actual contract: each rung varies the INPUT, and a rung that
    /// sends the same bytes to a different model is prayer. Rung 3 used to be
    /// byte-identical to rung 1 (it re-truncated an already-shorter bundle at a
    /// ceiling it could never reach), so a rejection caused by evidence that
    /// had been truncated away could never be recovered and the file rode to
    /// SLM_FAIL when a wider bundle would have named it. Being a plain function
    /// of the config and the Evidence, it is also the part that can be proved
    /// without a live llama-server.
    fn rung(&self, attempt: u8, ev: &Evidence) -> (Tier, String) {
        let budget = self.cfg.evidence_token_budget;
        match attempt {
            1 => (Tier::Primary, ev.bundle.clone()),
            // Attempt 2: primary again, evidence trimmed to 5a-only. If a
            // validator rejected attempt 1, the caller quotes the violation.
            2 => (Tier::Primary, filter::trimmed_bundle(ev, budget)),
            // Attempt 3: escalate to Qwen3-1.7B AND widen the evidence.
            _ => (Tier::Escalation, filter::widened_bundle(ev, budget * 2)),
        }
    }

    async fn name_with_retries(
        &self,
        sha: &str,
        ev: &Evidence,
        checker: &Checker,
        meta_dates: &[String],
        modified_iso: &str,
        ettin_date: Option<&str>,
    ) -> Option<crate::checker::Validated> {
        let mut violation: Option<String> = None;
        // No classifier ran means no type to declare; saying so beats naming a
        // type the sidecar never actually decided on.
        let doc_type_hint = ev.doc_type.as_deref().unwrap_or("unknown");
        for attempt in 1..=self.cfg.max_stage_attempts {
            let (tier, bundle) = self.rung(attempt, ev);
            let out = self
                .slm
                .name_document(
                    tier,
                    &bundle,
                    doc_type_hint,
                    &ev.language,
                    violation.as_deref(),
                )
                .await;
            match out {
                Ok(o) => match checker.check(&o, &ev.harvest, meta_dates, modified_iso, ettin_date)
                {
                    Ok(mut v) => {
                        // Ettin/SLM hard disagreement path: one re-prompt with
                        // spans pinned; after that it stays a soft flag.
                        let span_mismatch =
                            v.soft_flags.iter().any(|f| f.starts_with("SPAN_MISMATCH"));
                        if let (true, Some(ed)) = (span_mismatch && attempt == 1, ettin_date) {
                            violation = Some(format!(
                                "your date disagrees with a high-confidence extracted DATE span ({ed}); re-examine the evidence spans"
                            ));
                            let _ =
                                self.ledger
                                    .log_event(sha, "name", "span mismatch, re-prompting");
                            // keep v as a fallback if the retry also mismatches
                            let retry = self
                                .slm
                                .name_document(
                                    tier,
                                    &bundle,
                                    doc_type_hint,
                                    &ev.language,
                                    violation.as_deref(),
                                )
                                .await;
                            if let Ok(o2) = retry {
                                if let Ok(v2) = checker.check(
                                    &o2,
                                    &ev.harvest,
                                    meta_dates,
                                    modified_iso,
                                    ettin_date,
                                ) {
                                    if !v2.soft_flags.iter().any(|f| f.starts_with("SPAN_MISMATCH"))
                                    {
                                        return Some(v2);
                                    }
                                }
                            }
                            v.soft_flags.push("SPAN_MISMATCH_PERSISTED".into());
                        }
                        let _ = self.advance(sha, JobState::Named);
                        return Some(v);
                    }
                    Err(ce) => {
                        // Full message (with the offending text) drives the
                        // on-device re-prompt; the persisted log gets the code.
                        violation = Some(ce.to_string());
                        let _ = self.ledger.log_event(
                            sha,
                            "validate",
                            &format!("attempt {attempt} rejected: {}", ce.code()),
                        );
                        if matches!(ce, CheckError::TooLong(_, _)) && attempt >= 2 {
                            // Length problems rarely improve with escalation;
                            // ask for a shorter subject explicitly.
                            violation = Some("subject too long; use at most 6 short words".into());
                        }
                    }
                },
                Err(e) => {
                    violation = None; // model failure, not a validation failure
                                      // The error body can quote the model's raw proposed subject
                                      // and description; persist the code, log the rest.
                    log::warn!("name attempt {attempt} SLM error: {e}");
                    let _ = self.ledger.log_event(
                        sha,
                        "name",
                        &format!("attempt {attempt} SLM error: {}", error_code(&e)),
                    );
                }
            }
        }
        None
    }

    async fn handle_duplicate(
        &self,
        sha: &str,
        path: &Path,
        name: &str,
        ext: &str,
        existing: &crate::ledger::Job,
        clock: &WorkClock,
    ) {
        let _ = self
            .ledger
            .log_event(sha, "ingest", "duplicate content detected");
        if existing.state != JobState::Emitted {
            return; // duplicate of a flagged file: nothing sane to emit
        }
        let Some(orig_final) = existing.final_filename.clone() else {
            return;
        };

        // Identity for this *physical* copy: the manifest_id, a 64-hex hash of
        // content + this copy's path. Deterministic so replaying the same file
        // is idempotent at Flow 2's manifest_id gate, and distinct per copy so
        // each gets its own " (n)" row.
        let rel = relpath(&self.cfg.processing_dir, path);
        let dup_key = manifest_id(sha, &rel);

        // If this copy already produced a manifest, re-emit the identical one
        // (same key, same name) instead of inventing a new " (n)".
        let prior_name = match self.ledger.get(&dup_key) {
            Ok(job) => job.and_then(|j| j.final_filename),
            Err(e) => {
                log::error!("duplicate ledger lookup failed: {e}");
                return;
            }
        };
        let final_filename = match prior_name {
            Some(n) => n,
            None => {
                let stem = orig_final
                    .rsplit_once('.')
                    .map(|(s, _)| s)
                    .unwrap_or(&orig_final);
                // Persist the copy as a terminal row so later copies increment
                // past it (otherwise every distinct copy resolves to " (2)"
                // and collides in Flow 2's Archive copy). The row has to exist
                // before the name can be reserved onto it.
                match self.record_duplicate(&dup_key, path, name, &rel, ext, stem, &orig_final) {
                    Ok(fname) => fname,
                    Err(e) => {
                        log::error!("duplicate ledger record failed for {sha}: {e}");
                        return;
                    }
                }
            }
        };

        clock.parked(self.pacer.permit()).await;
        let m = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: dup_key.clone(),
            sha256: sha.to_string(),
            status: "ok".into(),
            original_name: name.into(),
            original_relpath: rel,
            new_filename: Some(final_filename),
            description: existing.description.clone(),
            date: existing.proposed_date.clone(),
            date_source: existing.date_source.clone(),
            doc_type: existing.doc_type.clone(),
            language: existing.language.clone(),
            duplicate_of: Some(sha.to_string()),
            soft_flags: vec!["DUPLICATE_CONTENT".into()],
            flag_reason: None,
            model_versions: self.model_versions.clone(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        match write_manifest(&self.cfg.manifests_dir(), &m) {
            Ok(_) => {
                let _ = self
                    .ledger
                    .log_event(sha, "emit", "duplicate manifest written");
            }
            Err(e) => {
                log::error!("duplicate manifest write failed for {dup_key}: {e}");
                let _ = self.ledger.log_event(
                    sha,
                    "emit",
                    &format!("duplicate manifest write FAILED: {e}"),
                );
            }
        }
    }

    /// Durable, terminal ledger row for a physical duplicate copy so
    /// `reserve_name` sees its filename and later copies resolve to the next
    /// " (n)". Keyed by the copy's deterministic duplicate id, not the shared
    /// content hash. Returns the name it reserved.
    #[allow(clippy::too_many_arguments)] // one copy's identity, spelled out
    fn record_duplicate(
        &self,
        dup_key: &str,
        path: &Path,
        name: &str,
        rel: &str,
        ext: &str,
        stem: &str,
        orig_final: &str,
    ) -> anyhow::Result<String> {
        self.ledger
            .ingest(dup_key, &path.to_string_lossy(), name, rel, ext)?;
        let final_filename = self.ledger.reserve_name(stem, ext, dup_key)?;
        self.ledger.update_fields(
            dup_key,
            &[
                ("duplicate_of", Some(orig_final.to_string())),
                ("soft_flags", Some("DUPLICATE_CONTENT".into())),
            ],
        )?;
        anyhow::ensure!(
            self.ledger.set_state(dup_key, JobState::Emitted)?,
            "duplicate row {dup_key} could not be marked emitted"
        );
        Ok(final_filename)
    }

    /// The Processing-relative path this job's identity was computed from.
    /// Read from the row written at ingest so a later change to the configured
    /// Processing folder cannot silently re-key an existing job; recomputed
    /// only for rows that predate the column.
    fn identity_relpath(&self, sha: &str, path: &Path) -> String {
        self.ledger
            .get(sha)
            .ok()
            .flatten()
            .and_then(|j| j.original_relpath)
            .unwrap_or_else(|| relpath(&self.cfg.processing_dir, path))
    }

    /// A quarantine destination that cannot collide with another flagged file.
    ///
    /// The leaf name alone discards the Processing-relative subdirectory that
    /// identity treats as meaningful everywhere else, and both move paths
    /// clobber (`rename` is MoveFileExW with MOVEFILE_REPLACE_EXISTING on
    /// Windows; the `copy` fallback truncates). Two flagged `scan.pdf`s
    /// therefore collapsed onto one entry: the first document was destroyed
    /// while its NeedsReview row still promised a human could review it, and
    /// `resubmit` then restored the survivor's bytes under the dead job's name.
    fn quarantine_dest(&self, mid: &str, path: &Path) -> PathBuf {
        let fname = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());
        let stem = &mid[..12];
        let mut dest = self.cfg.quarantine_dir.join(format!("{stem}__{fname}"));
        // Same instance id AND same leaf name means the same physical file, so
        // this loop should never run; never overwrite on the chance it isn't.
        let mut n = 1u32;
        while dest.exists() && n <= 500 {
            n += 1;
            dest = self
                .cfg
                .quarantine_dir
                .join(format!("{stem}__{n}__{fname}"));
        }
        dest
    }

    async fn flag(&self, sha: &str, path: &Path, reason: String, clock: &WorkClock) {
        // The guarded transition is the gate, so it goes first: a straggler
        // must not be able to retro-flag a document Flow 2 already archived,
        // and a second flagged manifest for the same delivery would fail
        // write_manifest's identity check anyway.
        match self.ledger.set_state(sha, JobState::Flagged) {
            Ok(true) => {}
            Ok(false) => {
                log::warn!(
                    "not flagging {sha} ({reason}): the job is already resolved or owned elsewhere"
                );
                return;
            }
            Err(e) => {
                log::error!("could not flag {sha}: {e}");
                return;
            }
        }

        let original_relpath = self.identity_relpath(sha, path);
        let mid = manifest_id(sha, &original_relpath);

        // Move to local quarantine. Never lose the file: move, or copy then
        // remove the source (cross-volume rename fails). Surface a hard failure
        // instead of silently leaving the file orphaned in Processing while the
        // manifest claims it was quarantined.
        let _ = std::fs::create_dir_all(&self.cfg.quarantine_dir);
        let mut quarantined: Option<String> = None;
        if path.exists() {
            let dest = self.quarantine_dest(&mid, path);
            let moved = std::fs::rename(path, &dest).is_ok()
                || match std::fs::copy(path, &dest) {
                    Ok(_) => {
                        let _ = std::fs::remove_file(path);
                        true
                    }
                    Err(e) => {
                        log::error!("failed to quarantine a flagged file: {e}");
                        let _ = self.ledger.log_event(sha, "flag", "QUARANTINE_FAILED");
                        false
                    }
                };
            if moved {
                quarantined = Some(dest.to_string_lossy().into_owned());
            }
        }

        // Persist where the file actually went; `resubmit` reads this column
        // rather than reconstructing a name that was never unique.
        let _ = self.ledger.update_fields(
            sha,
            &[
                ("flag_reason", Some(reason.clone())),
                ("quarantine_path", quarantined),
            ],
        );
        let _ = self.ledger.log_event(sha, "flag", &reason);

        clock.parked(self.pacer.permit()).await;
        let m = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: mid,
            sha256: sha.to_string(),
            status: "flagged".into(),
            original_name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .into(),
            original_relpath,
            new_filename: None,
            description: None,
            date: None,
            date_source: None,
            doc_type: None,
            language: None,
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: Some(reason),
            model_versions: self.model_versions.clone(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = write_manifest(&self.cfg.manifests_dir(), &m) {
            log::error!("flagged manifest write failed for {sha}: {e}");
        }
        self.emit_update(sha);
    }

    /// Human correction from the review pane: re-validate and re-emit.
    pub async fn resubmit(
        &self,
        sha: &str,
        date: String,
        subject: String,
        description: String,
    ) -> anyhow::Result<()> {
        let job = self
            .ledger
            .get(sha)?
            .ok_or_else(|| anyhow::anyhow!("unknown job"))?;
        let md = std::fs::read_to_string(self.cfg.cache_dir.join(format!("{sha}.md")))
            .unwrap_or_default();
        let h = crate::harvest::harvest(&md);
        let checker = Checker::new(self.cfg.max_filename_len);

        // Human dates are trusted even if absent from evidence: bypass the
        // presence tripwire by injecting the date as metadata, but keep every
        // other rule (range, sanitization, sentence shape) fully enforced.
        let out = crate::checker::SlmOutput {
            date: date.clone(),
            date_source: "document".into(),
            subject,
            description,
        };
        let today = chrono::Utc::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        // `check_human`, not `check`: this is a correction a person typed in
        // the review pane after reading the document, so the model-style rules
        // (word count, the forward-date ceiling) are theirs to overrule. Every
        // safety rule still applies. Routing this through `check` applied the
        // model's rules to the human's answer, which meant the one surface a
        // user is left alone with could refuse the correct name and leave the
        // file with no path forward.
        let v = checker.check_human(&out, &h, &[date], &today, None)?;

        // ONE value for the file's identity, not two independent
        // reconstructions: the flagged manifest's id, its `original_relpath`,
        // and the location the document is restored to all derive from the
        // relpath recorded at ingest. Deriving the restore path from the leaf
        // name instead is what made replace_flagged_manifest's identity
        // assertion fail for every file that lived in a Processing subfolder.
        let original_relpath = job
            .original_relpath
            .clone()
            .unwrap_or_else(|| relpath(&self.cfg.processing_dir, Path::new(&job.original_path)));
        let mid = manifest_id(sha, &original_relpath);

        // The reservation is a ledger write, so it is the one mutation that
        // must precede the manifest; it is rolled back below if the write
        // fails, leaving the job exactly as the operator found it.
        let previous_final = job.final_filename.clone();
        let final_filename = self.ledger.reserve_name(&v.base_name, &job.ext, sha)?;

        let m = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: mid,
            sha256: sha.to_string(),
            status: "ok".into(),
            original_name: job.original_name.clone(),
            original_relpath: original_relpath.clone(),
            new_filename: Some(final_filename.clone()),
            description: Some(v.description.clone()),
            date: Some(v.date_iso.clone()),
            date_source: Some("human".into()),
            doc_type: job.doc_type,
            language: job.language,
            duplicate_of: None,
            soft_flags: vec!["HUMAN_CORRECTED".into()],
            flag_reason: None,
            model_versions: self.model_versions.clone(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        // Nothing else commits until the manifest is on disk. The old order
        // cleared flag_reason, logged the correction and un-quarantined the
        // document first, so a failed write left a flagged row with no reason,
        // a pending flagged manifest, and the file back in Processing where
        // the watcher re-ingested it as a spurious duplicate.
        if let Err(e) = write_manifest(&self.cfg.manifests_dir(), &m) {
            let _ = self
                .ledger
                .update_fields(sha, &[("final_filename", previous_final)]);
            let _ = self.ledger.log_event(
                sha,
                "resubmit",
                "correction rejected: manifest write failed",
            );
            return Err(e);
        }

        // The guarded Flagged -> Emitted swap is the FIRST ledger mutation after
        // the write, because it is the one that can still refuse. Clearing
        // flag_reason ahead of it only moved the half-commit three lines later:
        // a job dismissed or re-flagged between the `get` above and here left an
        // `ok` manifest in the Outbox, flag_reason NULL and the row still
        // Flagged — a NeedsReview card with no reason on it, which is the exact
        // symptom this ordering exists to prevent. Roll the reservation back the
        // same way the write-failure branch does, so the only two outcomes stay
        // "fully corrected" and "untouched, still flagged with its reason".
        let owned = self.ledger.set_state(sha, JobState::Emitted);
        if !matches!(owned, Ok(true)) {
            let _ = self
                .ledger
                .update_fields(sha, &[("final_filename", previous_final)]);
            let _ = self.ledger.log_event(
                sha,
                "resubmit",
                "correction abandoned: the job is no longer flagged",
            );
            owned?;
            anyhow::bail!("job {sha} is no longer flagged; nothing to correct");
        }
        self.ledger.update_fields(
            sha,
            &[
                ("proposed_date", Some(v.date_iso)),
                ("date_source", Some("human".into())),
                ("proposed_subject", Some(v.subject)),
                ("description", Some(v.description)),
                ("flag_reason", None),
                ("soft_flags", Some("HUMAN_CORRECTED".into())),
            ],
        )?;
        self.ledger
            .log_event(sha, "resubmit", "human correction accepted")?;

        // Quarantined original moves back into scope for Flow 2's rename — to
        // the relative location its identity was computed from, not to the
        // Processing root under its leaf name.
        let quarantined = job
            .quarantine_path
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cfg.quarantine_dir.join(&job.original_name));
        if quarantined.exists() {
            let back = self.cfg.processing_dir.join(&original_relpath);
            if let Some(parent) = back.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::rename(&quarantined, &back) {
                log::error!("corrected file could not leave quarantine: {e}");
                let _ = self.ledger.log_event(sha, "resubmit", "RESTORE_FAILED");
            } else {
                let _ = self.ledger.update_fields(sha, &[("quarantine_path", None)]);
            }
        }

        self.purge_cache(sha);
        self.emit_update(sha);
        Ok(())
    }

    /// Delete a job's cached raw markdown unless the operator opted into
    /// corpus retention. Keeps document text off disk once a file is resolved;
    /// flagged files awaiting human review are purged only after resubmit.
    fn purge_cache(&self, sha: &str) {
        if self.cfg.retain_cache {
            return;
        }
        let cache = self.cfg.cache_dir.join(format!("{sha}.md"));
        let _ = std::fs::remove_file(cache);
    }
}

pub fn hash_file(path: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn relpath(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Startup sweep of orphaned document text. Deletes only entries whose job is
/// genuinely finished (emitted/dismissed) or gone from the ledger entirely; a
/// flagged or still in-flight job keeps its evidence regardless of age.
///
/// The ledger is what makes that distinction possible, and the version of this
/// function without one deleted on mtime alone: a document sitting in
/// NeedsReview over a two-week holiday lost its evidence pane exactly when the
/// human finally opened it, and the review card then blamed the wrong thing —
/// "(no cached text; file failed before conversion)" for a document that
/// converted perfectly.
///
/// It deliberately does NOT touch the events table. Deleting a flagged job's
/// forensic trail — its OCR confidences, checker rejection codes and
/// span-mismatch re-prompts — from inside the function whose whole purpose is
/// preserving that job's evidence is self-contradictory, and it also silently
/// overrode the caller: `lib.rs` sweeps events on its own 30-day floor, which
/// this call (driven by `cache_ttl_days`, default 7) pre-empted every time.
pub fn sweep_cache_with_ledger(cache_dir: &Path, ttl_days: u64, ledger: &Ledger) {
    if cache_dir.as_os_str().is_empty() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(
        ttl_days.saturating_mul(86_400),
    )) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Some(sha) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Anything the ledger still has an unresolved row for is evidence a
        // human is (or will be) looking at. An error reading the ledger fails
        // closed the same way.
        match ledger.job_state(sha) {
            Ok(Some(state)) if !state.is_resolved() => continue,
            Err(e) => {
                log::warn!("cache sweep skipping {sha}: ledger read failed: {e}");
                continue;
            }
            _ => {}
        }
        if let Ok(modified) = e.metadata().and_then(|m| m.modified()) {
            if modified < cutoff {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

/// Value-free classification of a runtime error for the persisted audit trail.
/// The full message goes to the app log; the ledger gets a stable code,
/// because sidecar errors embed the document's absolute path and SLM errors
/// embed the model's raw proposed subject and description — exactly the PII
/// the ingest event deliberately keeps out of the events table.
fn error_code(e: &anyhow::Error) -> &'static str {
    let msg = e.to_string().to_ascii_lowercase();
    if msg.contains("panicked") {
        "PANIC"
    } else if msg.contains("timed out") || msg.contains("timeout") {
        "TIMEOUT"
    } else if msg.contains("password") || msg.contains("encrypted") {
        "ENCRYPTED"
    } else {
        "ERROR"
    }
}

/// The manifest id / instance identity for a file at `relpath` with the given
/// content hash: a filesystem-safe 64-hex value that is the manifest's
/// idempotency key and filename. See `crate::identity`.
fn manifest_id(content_sha: &str, relpath: &str) -> String {
    crate::identity::instance_id(content_sha, &crate::identity::normalize_relpath(relpath))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_id_is_fs_safe_and_path_case_insensitive() {
        let sha = "a".repeat(64);
        let id = manifest_id(&sha, "Sub\\Dir\\File.PDF");
        assert!(crate::identity::is_safe_identifier(&id));
        // Separator/case-insensitive: the same physical file yields one id.
        assert_eq!(id, manifest_id(&sha, "sub/dir/file.pdf"));
        // Distinct copies get distinct ids so each gets its own " (n)" row.
        assert_ne!(
            manifest_id(&sha, "a/one.pdf"),
            manifest_id(&sha, "a/two.pdf")
        );
    }

    // ---- async-hygiene regression coverage ---------------------------------
    // process_inner no longer calls hash_file/routing::detect/sidecar ops
    // directly; it moves owned args into tokio::task::spawn_blocking and
    // folds a JoinError into the same anyhow::Result the direct call would
    // have returned. These tests pin down that exact idiom.

    #[tokio::test]
    async fn hash_file_matches_when_run_via_spawn_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sample.txt");
        std::fs::write(&file, b"backlog pipeline async hygiene").unwrap();

        let direct = hash_file(&file).unwrap();

        // Same call pattern process_inner now uses: an owned path clone moved
        // into spawn_blocking, JoinError folded into an anyhow::Result.
        let moved = file.clone();
        let via_pool: anyhow::Result<String> =
            match tokio::task::spawn_blocking(move || hash_file(&moved)).await {
                Ok(r) => r,
                Err(join_err) => Err(anyhow::anyhow!("hash task panicked: {join_err}")),
            };
        assert_eq!(direct, via_pool.unwrap());
    }

    #[tokio::test]
    async fn spawn_blocking_panic_yields_joinerror_not_a_worker_panic() {
        // Proves the JoinError-folding idiom used at every wrapped call site
        // (hash_file, routing::detect, pdf_probe, convert/ocr, build_evidence)
        // observes a panicking blocking task as a plain Err rather than the
        // panic propagating into (and killing) the calling async task.
        let handle: tokio::task::JoinHandle<()> =
            tokio::task::spawn_blocking(|| panic!("simulated poison-pill blocking task"));
        let outcome = handle.await;
        assert!(
            outcome.is_err(),
            "a panicking spawn_blocking task must surface as Err(JoinError)"
        );
    }

    // ---- orchestrator behaviour -------------------------------------------
    // Driven headlessly: `Pipeline::app` is None (no Tauri app to build one
    // from) and the sidecar/SLM binaries are paths that never spawn, so every
    // test below exercises ledger + filesystem behaviour only.

    /// A throwaway clock for the direct `flag` calls below. Nothing in these
    /// tests queues, so the accounting it carries is never consulted.
    fn clock() -> WorkClock {
        WorkClock::default()
    }

    struct Harness {
        /// Read only by the `#[cfg(unix)]` tests below, but load-bearing on
        /// every platform: `TempDir` deletes its directory on drop, so this
        /// field is what keeps the config's `processing_dir` and friends alive
        /// for as long as the harness is. Dropping it would delete the tree the
        /// pipeline is pointed at. `allow` rather than `expect` because on unix
        /// it *is* read and `expect` would then fire in the other direction.
        #[allow(dead_code)]
        dir: tempfile::TempDir,
        pipeline: Arc<Pipeline>,
    }

    impl Harness {
        fn new() -> Self {
            Self::with(|_| {})
        }

        fn with(tweak: impl FnOnce(&mut Config)) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let mut cfg = Config {
                processing_dir: root.join("processing"),
                outbox_dir: root.join("outbox"),
                quarantine_dir: root.join("quarantine"),
                cache_dir: root.join("cache"),
                manifest_emit_per_min: 0,
                ..Default::default()
            };
            tweak(&mut cfg);
            for d in [
                &cfg.processing_dir,
                &cfg.outbox_dir,
                &cfg.quarantine_dir,
                &cfg.cache_dir,
            ] {
                std::fs::create_dir_all(d).unwrap();
            }
            let ledger = Arc::new(Ledger::open(&root.join("ledger.db")).unwrap());
            let sidecar = Arc::new(Sidecar::new(root.join("no-such-convertd")));
            let slm = Arc::new(SlmLane::new(
                root.join("no-such-llama-server"),
                String::new(),
                root.join("primary.gguf"),
                root.join("escalation.gguf"),
                18137,
                1,
            ));
            let pipeline = Arc::new(Pipeline {
                convert_slots: Arc::new(Semaphore::new(1)),
                slm_slots: Arc::new(Semaphore::new(1)),
                ingest_slots: Arc::new(Semaphore::new(8)),
                inflight: Arc::new(Mutex::new(HashSet::new())),
                pacer: Arc::new(Pacer::new(0)),
                cfg,
                ledger,
                sidecar,
                slm,
                app: None,
                paused: Arc::new(AtomicBool::new(false)),
                // Non-empty on purpose: `Manifest::validate` refuses an `ok`
                // manifest with no provenance, and a real run always has some
                // (see the `sidecar.versions()` snapshot in `Pipeline::new`).
                model_versions: json!({ "convertd": "test" }),
            });
            Self { dir, pipeline }
        }

        /// Create a document at `rel` under Processing and give it an ingested
        /// ledger row, exactly as `process_inner` would have.
        fn seed(&self, rel: &str, body: &str) -> (String, PathBuf) {
            let path = self.pipeline.cfg.processing_dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            let sha = hash_file(&path).unwrap();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            self.pipeline
                .ledger
                .ingest(&sha, &path.to_string_lossy(), &name, rel, "pdf")
                .unwrap();
            (sha, path)
        }

        fn manifest(&self, sha: &str, rel: &str) -> Option<Manifest> {
            let p = self
                .pipeline
                .cfg
                .manifests_dir()
                .join(format!("{}.json", manifest_id(sha, rel)));
            std::fs::read(p)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
        }

        fn quarantine_entries(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.pipeline.cfg.quarantine_dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    /// The smallest honest cap this config can express: one second per sidecar
    /// round-trip, one naming rung. `wall_clock_cap` is deliberately the SUM of
    /// the stage timeouts it wraps, so a test shrinks those rather than
    /// shrinking the cap out from under them.
    ///
    /// Gated to match its callers: both tests that use it drive a shell-script
    /// stand-in for convertd and are themselves `#[cfg(unix)]`, so on Windows
    /// this is genuinely unreachable rather than merely unused.
    #[cfg(unix)]
    fn tiny_cap(cfg: &mut Config) {
        cfg.per_file_wall_clock_secs = 1;
        cfg.sidecar_timeout_secs = 1;
        cfg.max_stage_attempts = 1;
    }

    /// Stands in for convertd: same newline-delimited JSON protocol, answers
    /// `convert` with the document's own bytes.
    #[cfg(unix)]
    const FAKE_CONVERTD: &str = r##"#!/usr/bin/env python3
import json, sys

while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    op = req.get("op")
    out = {"id": req["id"], "ok": True}
    if op in ("convert", "ocr"):
        with open(req["path"], encoding="utf-8") as handle:
            out["markdown"] = handle.read()
        out["doc_meta_dates"] = []
    elif op == "langid":
        out["lang"] = "en"
    elif op == "classify":
        out.update({"label": "correspondence", "score": 0.0, "available": False})
    elif op == "salience":
        out["indices"] = []
    elif op == "pdf_probe":
        out.update({"median_chars_per_page": 900, "pages": 1})
    else:
        out["spans"] = []
    sys.stdout.write(json.dumps(out) + "\n")
    sys.stdout.flush()
"##;

    /// Stands in for llama-server: `/health` plus one schema-shaped completion.
    /// Threaded because the naming lane keeps the health connection alive and
    /// opens a second one for the chat request.
    #[cfg(unix)]
    const FAKE_LLAMA_SERVER: &str = r##"#!/usr/bin/env python3
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

NAME = {
    "date": "2024-03-05",
    "date_source": "document",
    "subject": "Acme Corporation Invoice March",
    "description": "Invoice from Acme Corporation covering March consulting services.",
}

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def reply(self, payload):
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.reply({"status": "ok"})

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.reply({"choices": [{"message": {"content": json.dumps(NAME)}}]})

    def log_message(self, *args):
        pass

port = int(sys.argv[sys.argv.index("--port") + 1])
server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
server.daemon_threads = True
server.serve_forever()
"##;

    /// A document the deterministic checker can actually validate: the date the
    /// stub proposes is written in the body, so the anti-hallucination tripwire
    /// passes for the right reason.
    #[cfg(unix)]
    const QUEUED_DOCUMENT: &str = "SUBJECT: Acme Corporation Invoice March\n\n\
         2024-03-05\n\n\
         Invoice from Acme Corporation covering March consulting services \
         rendered under the master services agreement.\n\n\
         Sincerely,\nA. Person\n";

    /// P1's other half, and the reason the cap had to be re-dimensioned before
    /// it was allowed to quarantine anything. The timed region wrapped
    /// `convert_slots`, `slm_slots` AND the emit pacer — all three backpressure
    /// mechanisms — so a file that merely QUEUED behind other files blew the
    /// cap. On a 4-core box the defaults admit 8 files against 2 convert slots
    /// and `manifest_emit_per_min` parks emissions on purpose, which makes this
    /// the normal shape of a backfill rather than an edge case; the cost is now
    /// a false quarantine, a `flagged` manifest shipped to SharePoint and a
    /// frozen row, not a log line.
    ///
    /// The two binaries this box cannot run are stubbed: convertd over its own
    /// line protocol, llama-server over `/health` + one chat completion.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_queued_past_the_whole_cap_still_reaches_emitted() {
        use std::os::unix::fs::PermissionsExt;

        let h = Harness::with(tiny_cap);
        let cap = wall_clock_cap(&h.pipeline.cfg);
        let script = |name: &str, body: &str| {
            let p = h.dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let convertd = script("fake-convertd", FAKE_CONVERTD);
        let llama = script("fake-llama-server", FAKE_LLAMA_SERVER);
        for gguf in ["primary.gguf", "escalation.gguf"] {
            std::fs::write(h.dir.path().join(gguf), b"").unwrap();
        }

        let convert_slots = Arc::new(Semaphore::new(1));
        let pipeline = Arc::new(Pipeline {
            sidecar: Arc::new(Sidecar::with_timeout(
                convertd,
                std::time::Duration::from_secs(cap),
            )),
            slm: Arc::new(SlmLane::new(
                llama,
                String::new(),
                h.dir.path().join("primary.gguf"),
                h.dir.path().join("escalation.gguf"),
                18937,
                1,
            )),
            cfg: h.pipeline.cfg.clone(),
            ledger: h.pipeline.ledger.clone(),
            app: None,
            paused: h.pipeline.paused.clone(),
            convert_slots: convert_slots.clone(),
            slm_slots: Arc::new(Semaphore::new(1)),
            ingest_slots: Arc::new(Semaphore::new(8)),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            pacer: Arc::new(Pacer::new(0)),
            model_versions: json!({ "convertd": "test" }),
        });

        let rel = "vendor/statement.txt";
        let path = pipeline.cfg.processing_dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, QUEUED_DOCUMENT).unwrap();
        let sha = hash_file(&path).unwrap();

        // Hold the only convert slot for longer than the entire cap. Every
        // second of this is queue time, and none of it is the file's own work.
        let holder = convert_slots.clone().acquire_owned().await.unwrap();
        let run = tokio::spawn(pipeline.clone().process_file(path.clone()));
        tokio::time::sleep(std::time::Duration::from_secs(cap + 1)).await;
        drop(holder);
        run.await.unwrap();

        let job = pipeline
            .ledger
            .get(&sha)
            .unwrap()
            .expect("the job must exist");
        assert_eq!(
            job.state,
            JobState::Emitted,
            "a file that only queued must not be flagged (reason: {:?})",
            job.flag_reason
        );
        assert!(job.flag_reason.is_none());
        let m = h
            .manifest(&sha, rel)
            .expect("a queued file must still deliver its manifest");
        assert_eq!(m.status, "ok");
        assert_eq!(
            m.new_filename.as_deref(),
            Some("2024-03-05 Acme Corporation Invoice March.txt")
        );
        assert!(
            path.exists(),
            "an emitted file stays in Processing for Flow 2"
        );
    }

    /// P1: the wall-clock cap used to log a line and walk away, leaving the
    /// file in Processing, the row parked mid-ladder and no manifest at all.
    /// The sidecar here is a script that never answers, so `convert` blocks
    /// past the cap for real.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_file_is_quarantined_with_a_timeout_reason() {
        use std::os::unix::fs::PermissionsExt;

        let h = Harness::with(tiny_cap);
        // A "sidecar" that accepts the request and never replies: convert
        // blocks on the response until its own deadline expires, which is set
        // past the wall-clock cap on purpose.
        let fake = h.dir.path().join("slow-convertd");
        std::fs::write(&fake, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pipeline = Arc::new(Pipeline {
            sidecar: Arc::new(Sidecar::with_timeout(
                fake,
                std::time::Duration::from_secs(wall_clock_cap(&h.pipeline.cfg) + 2),
            )),
            cfg: h.pipeline.cfg.clone(),
            ledger: h.pipeline.ledger.clone(),
            slm: h.pipeline.slm.clone(),
            app: None,
            paused: h.pipeline.paused.clone(),
            convert_slots: Arc::new(Semaphore::new(1)),
            slm_slots: Arc::new(Semaphore::new(1)),
            ingest_slots: Arc::new(Semaphore::new(8)),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            pacer: Arc::new(Pacer::new(0)),
            model_versions: json!({}),
        });

        let rel = "vendor/statement.txt";
        let path = pipeline.cfg.processing_dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "a plain text document body long enough to convert").unwrap();
        let sha = hash_file(&path).unwrap();

        pipeline.clone().process_file(path.clone()).await;

        let job = pipeline
            .ledger
            .get(&sha)
            .unwrap()
            .expect("the job must exist");
        assert_eq!(
            job.state,
            JobState::Flagged,
            "a timed-out job must not stay mid-ladder"
        );
        let reason = job
            .flag_reason
            .expect("a timed-out job must carry a reason");
        assert!(
            reason.starts_with("TIMEOUT:"),
            "unexpected reason: {reason}"
        );
        // The concrete stage, not just the words "at stage": `last_stage` is
        // written only after a stage SUCCEEDS, so reading it named the previous
        // rung — this job is at state `ingested` and dies inside convert.
        assert!(
            reason.ends_with("at stage convert"),
            "the reason must name the stage the file actually died in: {reason}"
        );

        // The operator's three surfaces: quarantine, NeedsReview, Outbox.
        assert!(!path.exists(), "the file must leave Processing");
        assert!(
            job.quarantine_path.is_some(),
            "quarantine location must be recorded"
        );
        assert!(PathBuf::from(job.quarantine_path.unwrap()).exists());
        let m = h
            .manifest(&sha, rel)
            .expect("a flagged manifest must be written");
        assert_eq!(m.status, "flagged");
        assert!(m.flag_reason.unwrap().starts_with("TIMEOUT:"));
        // The claim is released even though the future was dropped mid-flight.
        assert!(pipeline
            .ledger
            .get(&sha)
            .unwrap()
            .unwrap()
            .claimed_at
            .is_none());
    }

    /// P3: two flagged documents sharing a leaf name must both survive. The
    /// old code moved each to `quarantine/<leaf>`, so the first was destroyed
    /// while its NeedsReview row still promised a human could review it.
    #[tokio::test]
    async fn two_flagged_files_with_one_basename_both_survive_quarantine() {
        let h = Harness::new();
        let (sha_a, path_a) = h.seed("acme/scan.pdf", "first document, acme");
        let (sha_b, path_b) = h.seed("zenith/scan.pdf", "second document, zenith");

        h.pipeline
            .flag(&sha_a, &path_a, "UNREADABLE:test".into(), &clock())
            .await;
        h.pipeline
            .flag(&sha_b, &path_b, "UNREADABLE:test".into(), &clock())
            .await;

        let entries = h.quarantine_entries();
        assert_eq!(
            entries.len(),
            2,
            "both documents must be in quarantine: {entries:?}"
        );

        let job_a = h.pipeline.ledger.get(&sha_a).unwrap().unwrap();
        let job_b = h.pipeline.ledger.get(&sha_b).unwrap().unwrap();
        let qa = PathBuf::from(job_a.quarantine_path.unwrap());
        let qb = PathBuf::from(job_b.quarantine_path.unwrap());
        assert_ne!(qa, qb);
        assert_eq!(
            std::fs::read_to_string(&qa).unwrap(),
            "first document, acme"
        );
        assert_eq!(
            std::fs::read_to_string(&qb).unwrap(),
            "second document, zenith"
        );

        // Each gets its own flagged manifest, keyed by its own instance id.
        assert!(h.manifest(&sha_a, "acme/scan.pdf").is_some());
        assert!(h.manifest(&sha_b, "zenith/scan.pdf").is_some());
    }

    /// P5 at the orchestrator boundary: a straggler must not be able to drag
    /// an archived document back into NeedsReview.
    #[tokio::test]
    async fn flag_refuses_a_job_that_already_emitted() {
        let h = Harness::new();
        let (sha, path) = h.seed("done.pdf", "already delivered");
        h.pipeline
            .ledger
            .set_state(&sha, JobState::Emitted)
            .unwrap();

        h.pipeline
            .flag(&sha, &path, "SLM_FAIL:late loser".into(), &clock())
            .await;

        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(job.state, JobState::Emitted);
        assert!(job.flag_reason.is_none());
        assert!(
            path.exists(),
            "an emitted file must not be moved to quarantine"
        );
        assert!(h.quarantine_entries().is_empty());
    }

    fn correction() -> (String, String, String) {
        (
            "2024-03-05".into(),
            "Acme Corporation Invoice March".into(),
            "Invoice from Acme Corporation covering March consulting services.".into(),
        )
    }

    /// P4: a file from a Processing subfolder is the case that failed every
    /// time — the restore used the leaf name while the manifest id was
    /// recomputed from the full original path, so the identity assertion in
    /// replace_flagged_manifest could never match.
    #[tokio::test]
    async fn resubmit_from_a_subfolder_emits_and_restores_to_its_own_folder() {
        let h = Harness::new();
        let rel = "clients/acme/invoice.pdf";
        let (sha, path) = h.seed(rel, "invoice body");
        h.pipeline
            .flag(&sha, &path, "SLM_FAIL:no valid output".into(), &clock())
            .await;
        assert!(!path.exists());

        let (date, subject, description) = correction();
        h.pipeline
            .resubmit(&sha, date, subject, description)
            .await
            .unwrap();

        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(job.state, JobState::Emitted);
        assert!(job.flag_reason.is_none());
        assert_eq!(
            job.final_filename.as_deref(),
            Some("2024-03-05 Acme Corporation Invoice March.pdf")
        );

        // The flagged manifest was replaced in place, keeping its identity.
        let m = h.manifest(&sha, rel).expect("manifest must exist");
        assert_eq!(m.status, "ok");
        assert_eq!(m.original_relpath, rel);
        assert!(m.flag_reason.is_none());

        // Restored where Flow 2 will look for it, not at the Processing root.
        assert!(
            path.exists(),
            "the document must return to its own subfolder"
        );
        assert!(!h.pipeline.cfg.processing_dir.join("invoice.pdf").exists());
        assert!(h.quarantine_entries().is_empty());
    }

    /// P4's other half: if the manifest write fails, nothing may be committed.
    /// Corrupting the pending flagged manifest makes `write_manifest` fail
    /// where it always could — while reading back the delivery it is replacing.
    #[tokio::test]
    async fn a_failed_resubmit_leaves_the_job_flagged_and_the_file_in_quarantine() {
        let h = Harness::new();
        let rel = "clients/acme/invoice.pdf";
        let (sha, path) = h.seed(rel, "invoice body");
        h.pipeline
            .flag(&sha, &path, "SLM_FAIL:no valid output".into(), &clock())
            .await;
        let pending = h
            .pipeline
            .cfg
            .manifests_dir()
            .join(format!("{}.json", manifest_id(&sha, rel)));
        assert!(pending.exists());
        std::fs::write(&pending, b"{ this is not a manifest").unwrap();

        let (date, subject, description) = correction();
        let err = h.pipeline.resubmit(&sha, date, subject, description).await;
        assert!(
            err.is_err(),
            "a failed manifest write must surface as an error"
        );

        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(job.state, JobState::Flagged);
        assert_eq!(job.flag_reason.as_deref(), Some("SLM_FAIL:no valid output"));
        assert!(
            job.final_filename.is_none(),
            "the name reservation must be rolled back"
        );
        assert!(!path.exists(), "the document must stay in quarantine");
        assert!(PathBuf::from(job.quarantine_path.unwrap()).exists());
    }

    /// P4's remaining half-commit window: `resubmit` reads the job, then
    /// reserves, writes and commits. If the row moved in between — an operator
    /// dismissed it, another worker re-flagged it — the old order had already
    /// cleared flag_reason and written the human's date and subject before the
    /// Emitted swap could refuse, leaving a flagged card with no reason on it.
    #[tokio::test]
    async fn a_resubmit_whose_job_moved_under_it_leaves_the_flag_reason_intact() {
        let h = Harness::new();
        let rel = "clients/acme/invoice.pdf";
        let (sha, path) = h.seed(rel, "invoice body");
        h.pipeline
            .flag(&sha, &path, "SLM_FAIL:no valid output".into(), &clock())
            .await;

        // The operator dismissed the file from another window between the read
        // and the commit. Flagged -> Dismissed is legal; Dismissed -> Emitted
        // is not, so the correction must abandon rather than half-land.
        assert!(h
            .pipeline
            .ledger
            .set_state(&sha, JobState::Dismissed)
            .unwrap());

        let (date, subject, description) = correction();
        assert!(
            h.pipeline
                .resubmit(&sha, date, subject, description)
                .await
                .is_err(),
            "a correction that cannot commit must surface as an error"
        );

        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(job.state, JobState::Dismissed);
        assert_eq!(
            job.flag_reason.as_deref(),
            Some("SLM_FAIL:no valid output"),
            "a NeedsReview row must never lose the reason it is flagged for"
        );
        assert!(
            job.final_filename.is_none(),
            "the name reservation must be rolled back"
        );
        assert!(job.proposed_subject.is_none());
        assert!(!path.exists(), "the document must stay in quarantine");
    }

    /// P7: the sweep used to delete every cache entry past its TTL on mtime
    /// alone, so a document that sat in NeedsReview over a holiday lost its
    /// evidence pane exactly when the human finally opened it.
    #[test]
    fn the_cache_sweep_keeps_flagged_evidence_and_removes_orphans() {
        let h = Harness::new();
        let cache = &h.pipeline.cfg.cache_dir;
        let ledger = &h.pipeline.ledger;

        for (sha, state) in [
            ("1111", Some(JobState::Flagged)),
            ("2222", Some(JobState::Converted)),
            ("3333", Some(JobState::Emitted)),
            ("4444", None), // orphan: no ledger row at all
        ] {
            std::fs::write(cache.join(format!("{sha}.md")), "# cached document text").unwrap();
            if let Some(state) = state {
                ledger
                    .ingest(sha, "C:/P/x.pdf", "x.pdf", "x.pdf", "pdf")
                    .unwrap();
                ledger.set_state(sha, state).unwrap();
            }
        }
        // A 0-day TTL puts the cutoff at "now": everything already written is
        // past it, so only the job state can spare a file.
        std::thread::sleep(std::time::Duration::from_millis(10));
        sweep_cache_with_ledger(cache, 0, ledger);

        assert!(
            cache.join("1111.md").exists(),
            "flagged evidence must survive its TTL"
        );
        assert!(
            cache.join("2222.md").exists(),
            "an in-flight job's text must survive"
        );
        assert!(
            !cache.join("3333.md").exists(),
            "a delivered job's text must go"
        );
        assert!(!cache.join("4444.md").exists(), "a genuine orphan must go");
    }

    /// The same function used to delete the events table on age alone, which
    /// contradicted its own contract twice over: a flagged job kept its cached
    /// text but lost the forensic trail — OCR confidences, checker rejection
    /// codes, span-mismatch re-prompts — that the review surface exists to
    /// show, and `cache_ttl_days` (7) silently overrode the 30-day floor
    /// `lib.rs` asks for on the line right after it calls this.
    #[test]
    fn the_cache_sweep_leaves_the_audit_trail_to_its_own_owner() {
        let h = Harness::new();
        let cache = &h.pipeline.cfg.cache_dir;
        let ledger = &h.pipeline.ledger;

        ledger
            .ingest("1111", "C:/P/x.pdf", "x.pdf", "x.pdf", "pdf")
            .unwrap();
        ledger.set_state("1111", JobState::Flagged).unwrap();
        ledger
            .log_event("1111", "convert", "attempt 1: ocr conf 0.31 below floor")
            .unwrap();
        ledger
            .log_event(
                "1111",
                "validate",
                "attempt 3 rejected: DATE_NOT_IN_EVIDENCE",
            )
            .unwrap();
        std::fs::write(cache.join("1111.md"), "# cached document text").unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));
        sweep_cache_with_ledger(cache, 0, ledger);

        assert_eq!(
            ledger.events_for("1111", 10).unwrap().len(),
            2,
            "a flagged job's evidence pane must keep the trail that explains it"
        );
        assert!(cache.join("1111.md").exists());

        // The TTL that governs events belongs to the caller, and it still works.
        assert_eq!(ledger.sweep_events(0).unwrap(), 2);
    }

    /// P2's cheap half: the same path enqueued twice must not be hashed twice.
    #[test]
    fn a_path_already_in_flight_cannot_be_reserved_again() {
        let h = Harness::new();
        let path = h.pipeline.cfg.processing_dir.join("busy.pdf");
        let first = h
            .pipeline
            .begin_path(&path)
            .expect("first reservation wins");
        assert!(h.pipeline.begin_path(&path).is_none());
        drop(first);
        assert!(
            h.pipeline.begin_path(&path).is_some(),
            "the guard must free the path"
        );
    }

    /// An Evidence carrying `body` and, when `salient` is non-empty, the 5d
    /// picks that only exist for a thin document.
    fn evidence_for(body: &str, salient: &[&str]) -> Evidence {
        Evidence {
            bundle: String::new(),
            language: "en".into(),
            doc_type: None,
            doc_type_score: 0.0,
            harvest: crate::harvest::harvest(body),
            meta_dates: vec!["2024-03-05".into()],
            salient: salient.iter().map(|s| s.to_string()).collect(),
            ettin_spans: Vec::new(),
            thin: !salient.is_empty(),
        }
    }

    /// P14: the ladder's contract is that each rung varies the INPUT. Rung 3
    /// used to re-truncate an already-shorter bundle at a ceiling it could
    /// never reach, so it sent byte-identical evidence to a different model —
    /// a rejection caused by material that had been truncated away could never
    /// be recovered, and the file rode to SLM_FAIL and quarantine.
    ///
    /// This drives the rung selection itself, not just the helper it calls.
    #[test]
    fn rung_three_escalates_the_model_and_widens_what_rung_one_saw() {
        let h = Harness::with(|cfg| cfg.evidence_token_budget = 200);
        let body = format!(
            "SUBJECT: Termination of the Acme services agreement\n\n{}\n\nSincerely, A. Person",
            "This paragraph restates the parties and the effective date. ".repeat(120)
        );
        let mut ev = evidence_for(&body, &[]);
        // What build_evidence would have produced at the configured budget.
        ev.bundle = filter::widened_bundle(&ev, h.pipeline.cfg.evidence_token_budget);

        let (tier_one, rung_one) = h.pipeline.rung(1, &ev);
        let (tier_three, rung_three) = h.pipeline.rung(3, &ev);

        assert_eq!(tier_one, Tier::Primary);
        assert_eq!(tier_three, Tier::Escalation);
        assert_ne!(
            rung_three, rung_one,
            "rung 3 must not send a byte-identical bundle to a different model"
        );
        assert!(
            rung_three.len() > rung_one.len(),
            "rung 3 must see material rung 1 did not ({} vs {})",
            rung_three.len(),
            rung_one.len()
        );
        assert!(
            rung_three.starts_with(&rung_one[..64]),
            "same document, more of it"
        );
    }

    /// Salience (5d) fires only when the deterministic harvest came back thin —
    /// exactly when KEY SENTENCES is the only substantive section in the
    /// bundle. Rebuilding the widened bundle with an empty `salient` therefore
    /// handed a thin document LESS evidence than the rung before it, which is
    /// the opposite of what the escalation is for.
    #[test]
    fn a_thin_documents_widened_bundle_keeps_its_key_sentences() {
        let h = Harness::new();
        let body = "Please be advised that the arrangement described below takes effect on \
                    2024-03-05 and continues until either party gives notice.";
        let ev = evidence_for(
            body,
            &[
                "The arrangement described below takes effect on 2024-03-05.",
                "It continues until either party gives notice.",
            ],
        );

        let (_, wide) = h.pipeline.rung(3, &ev);
        assert!(
            wide.contains("KEY SENTENCES:"),
            "the one section a thin document has must survive the widening: {wide}"
        );
        assert!(wide.contains("continues until either party gives notice"));
    }

    /// The budget is `evidence_token_budget * n * 4` — arithmetic with no
    /// relation to where the document's characters fall — so it lands mid
    /// codepoint routinely. `String::truncate` panics on such an index, and a
    /// panic here kills the task and strands the job with no flag at all.
    #[test]
    fn a_budget_landing_mid_codepoint_cuts_back_to_a_char_boundary() {
        // Three bytes per character on purpose: the budget advances in steps
        // of 4, so a 2-byte character would land every step on a boundary and
        // the search below would find nothing to test. With 3-byte characters
        // two thirds of the steps split one.
        let body = format!("SUBJECT: Reçu\n\n{}", "€".repeat(4000));
        let ev = evidence_for(&body, &[]);

        let full = filter::widened_bundle(&ev, 1_000_000);
        let budget = (16..full.len() / 4)
            .find(|tokens| !full.is_char_boundary(tokens * 4))
            .expect("a multibyte body must produce a budget that splits a codepoint");
        let cut = filter::widened_bundle(&ev, budget);

        assert_eq!(
            cut.len(),
            filter::floor_char_boundary(&full, budget * 4),
            "the bundle must be cut exactly at the last whole character"
        );
        assert!(
            cut.len() < budget * 4,
            "a split codepoint must cost bytes, not panic"
        );
        assert!(full.starts_with(&cut), "widening only adds material");
    }
}
