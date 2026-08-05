//! The orchestrator. Owns worker pools, drives each file through the state
//! machine, implements the §7 retry ladder (retries vary the input; identical
//! retries are prayer), and quarantines with machine-readable reasons.

use crate::checker::{fs_metadata_dates, CheckError, Checker};
use crate::config::Config;
use crate::filter::{self, Evidence};
use crate::ledger::{Job, JobState, Ledger};
use crate::local_output::{self, DeliverResult};
use crate::manifest::{write_manifest, Manifest, Pacer, MANIFEST_SCHEMA_VERSION};
use crate::routing::{self, Route};
use crate::sidecar::{ConvertResult, Sidecar};
use crate::slm::{SlmLane, Tier};
use serde_json::json;
use std::collections::HashSet;
use std::io::Write;
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

/// Physical duplicate rows are keyed by their per-path delivery id rather
/// than by the shared content hash. Manifests still describe the bytes, so a
/// duplicate's parent is always the immutable content hash, never the ledger
/// lookup key (or the previously reserved filename stored for display).
fn manifest_duplicate_of(job: &Job) -> Option<String> {
    (job.sha256 != job.content_sha256).then(|| job.content_sha256.clone())
}

/// A durable terminal artifact can complete the review operation that created
/// it after a crash, but cannot override a competing human decision. Check
/// this before Local recovery publishes/deletes files and again in the ledger
/// CAS that commits its fields.
fn ensure_recovery_operation_compatible(
    job: &Job,
    target: JobState,
    delivery: &str,
) -> anyhow::Result<()> {
    match (job.review_operation.as_deref(), target) {
        (None, _)
        | (Some("correct"), JobState::Emitted)
        | (Some("dismiss"), JobState::Dismissed) => Ok(()),
        (Some(operation), _) => anyhow::bail!(
            "{delivery} terminal artifact conflicts with review operation {operation}"
        ),
    }
}

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

/// A second physical copy is not allowed to claim the content-key row while
/// its original delivery is still live. Its one watcher event must therefore
/// stay alive for the operator-configured terminal window of that delivery.
/// Tests compress seconds into milliseconds so the same boundary is exercised
/// without making the suite wait a minute and a half.
fn deferred_duplicate_retry_window(cfg: &Config) -> std::time::Duration {
    if cfg!(test) {
        std::time::Duration::from_millis(cfg.per_file_wall_clock_secs.saturating_mul(10))
    } else {
        std::time::Duration::from_secs(cfg.per_file_wall_clock_secs)
    }
}

fn deferred_duplicate_retry_interval() -> std::time::Duration {
    if cfg!(test) {
        std::time::Duration::from_millis(10)
    } else {
        std::time::Duration::from_secs(2)
    }
}

/// `stop_pipeline_inner` closes this semaphore before sidecars are torn down.
/// Polling it makes a deferred duplicate exit promptly during shutdown without
/// adding a second runtime-wide cancellation primitive.
const DEFERRED_DUPLICATE_SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(100);

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
    fn configured_delivery(&self) -> (&'static str, String) {
        (
            self.cfg.output_mode.as_str(),
            self.cfg.active_output_dir().to_string_lossy().into_owned(),
        )
    }

    pub fn new(
        cfg: Config,
        ledger: Arc<Ledger>,
        sidecar: Arc<Sidecar>,
        slm: Arc<SlmLane>,
        app: tauri::AppHandle,
    ) -> Arc<Self> {
        // One transient failure here used to cost the whole session, silently.
        //
        // `Manifest::validate` refuses an `ok` manifest whose `model_versions` is
        // empty — provenance is not optional — but it refuses it at the *last*
        // step, after hashing, conversion, filtering and the entire naming ladder
        // have already run. So a single failed probe at construction meant every
        // file in the run did all of its work and then flagged as
        // `RUNTIME_FAIL:manifest`, for as long as the app stayed up, with nothing
        // anywhere saying why.
        //
        // The probe is retried now — `Sidecar` spawns its first worker on demand,
        // so the very first call can lose a race with a cold PyInstaller start —
        // and a final failure is logged at `error` with what it is about to cost,
        // rather than swallowed.
        let model_versions = {
            let mut probed = None;
            for attempt in 1..=3 {
                match sidecar.versions() {
                    Ok(v) => {
                        probed = Some(v);
                        break;
                    }
                    Err(e) => {
                        log::warn!("sidecar version probe attempt {attempt} failed: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
            }
            probed.unwrap_or_else(|| {
                log::error!(
                    "the document reader never reported its versions, so every manifest this \
                     session will fail provenance validation and its file will be flagged \
                     RUNTIME_FAIL:manifest. Restart BackLog once Settings -> Readiness is green."
                );
                json!({})
            })
        };
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
        if !crate::watcher::is_safe_path_under_root(&self.cfg.processing_dir, &path) {
            log::warn!("refusing an intake path that is outside Processing or uses a reparse point: {path:?}");
            return;
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
                None => {
                    // No content sha yet, so there is no ledger row to flag
                    // and no quarantine key — the classic shape is a OneDrive
                    // placeholder hydrating (or a file locked by another
                    // process) inside `hash_file`. The file itself is left
                    // untouched in Processing; make the stall VISIBLE instead
                    // of leaving only a log line nobody reads: a greppable
                    // code for support bundles, and an event the UI can
                    // surface as "a file is stuck before identification".
                    log::error!(
                        "STALL:pre-sha wall-clock cap blown for {path:?} before it was identified; \
                         the file remains in Processing (sync placeholder or a lock held by another process?)"
                    );
                    if let Some(app) = &self.app {
                        let _ = app.emit("pre-identify-stall", ());
                    }
                }
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
        if !crate::watcher::is_safe_path_under_root(&self.cfg.processing_dir, &path) {
            log::warn!("refusing an intake path that is outside Processing or uses a reparse point: {path:?}");
            return;
        }
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
        if !crate::watcher::is_safe_path_under_root(&self.cfg.processing_dir, &path) {
            log::warn!("intake path changed while hashing; refusing to continue: {path:?}");
            return;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let ext = routing::extension_of(&path);
        let original_relpath = relpath(&self.cfg.processing_dir, &path);
        let incoming_delivery_id = manifest_id(&sha, &original_relpath);

        let (delivery_mode, delivery_root) = self.configured_delivery();
        let resume_state = match self.ledger.ingest_with_delivery(
            &sha,
            &path.to_string_lossy(),
            &name,
            &original_relpath,
            &ext,
            delivery_mode,
            &delivery_root,
            &self.cfg.processing_dir.to_string_lossy(),
            &sha,
        ) {
            Ok(None) => JobState::Ingested, // new
            Ok(Some(existing)) => {
                let same_delivery = existing.delivery_id == incoming_delivery_id
                    && crate::identity::normalize_relpath(&existing.original_path)
                        == crate::identity::normalize_relpath(&path.to_string_lossy())
                    && !existing.source_root.is_empty()
                    && crate::identity::normalize_relpath(&existing.source_root)
                        == crate::identity::normalize_relpath(
                            &self.cfg.processing_dir.to_string_lossy(),
                        );
                if !same_delivery {
                    // Same content seen again under a *different* file: emit a
                    // duplicate manifest so PA can index " (2)". Compare the
                    // normalized Processing-relative paths, which is what
                    // identity means here — the old raw-absolute-string test
                    // reclassified every job the moment the Processing folder
                    // moved, and fired on P4's restore path every time.
                    if existing.state.is_resolved() || existing.state == JobState::Flagged {
                        self.handle_duplicate(&sha, &path, &name, &ext, &existing, &clock)
                            .await;
                    } else {
                        // A watcher may report the second physical copy only
                        // once. Keep an in-process retry alive until the
                        // original resolves, then emit its own duplicate
                        // row; never let it claim or delete under the first
                        // content-key row.
                        //
                        // The wait is bounded by the ORIGINAL's lifecycle,
                        // not by a deadline of this copy's own: the original
                        // always reaches a terminal state — its wall clock
                        // flags it if nothing else does — while a fixed
                        // deadline here counted queue time the original's
                        // clock deliberately excludes. Any batch that queued
                        // longer than the window therefore orphaned its
                        // copies in Processing with no ledger row, no flag,
                        // and no UI count. The window now only paces a
                        // diagnostic, never an abandonment.
                        let warn_after = tokio::time::Instant::now()
                            + deferred_duplicate_retry_window(&self.cfg);
                        let retry_interval = deferred_duplicate_retry_interval();
                        let mut next_retry = tokio::time::Instant::now();
                        let mut warned = false;
                        loop {
                            if self.ingest_slots.is_closed() {
                                log::debug!(
                                    "stopping deferred same-content duplicate retry during shutdown"
                                );
                                return;
                            }
                            let now = tokio::time::Instant::now();
                            if !warned && now >= warn_after {
                                log::warn!(
                                    "deferred same-content physical copy is still waiting past its terminal window; keeping it parked until the original resolves"
                                );
                                warned = true;
                            }
                            if now >= next_retry {
                                let Ok(Some(current)) = self.ledger.get(&sha) else {
                                    return;
                                };
                                if current.delivery_id != existing.delivery_id {
                                    return;
                                }
                                if current.state.is_resolved() || current.state == JobState::Flagged
                                {
                                    self.handle_duplicate(
                                        &sha, &path, &name, &ext, &current, &clock,
                                    )
                                    .await;
                                    return;
                                }
                                next_retry = now + retry_interval;
                            }
                            // This wait is exclusively for another delivery's
                            // terminal transition. Do not spend this copy's
                            // own wall-clock work budget on it.
                            let wait = (next_retry - now).min(DEFERRED_DUPLICATE_SHUTDOWN_POLL);
                            clock.parked(tokio::time::sleep(wait)).await;
                        }
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

        // `write_manifest` is atomic, but the process can still die in the
        // few instructions before the ledger reaches its terminal state.
        // The durable delivery is authoritative on replay, so do not ask a
        // non-deterministic model to invent a second answer for the same id.
        if self.recover_terminal_manifest(&sha, &original_relpath) {
            return;
        }

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
        // Spelled out rather than `format!("{route:?}").to_lowercase()`: that
        // made a persisted ledger value depend on the enum's `Debug` output, so
        // renaming a variant would silently change what is already stored with no
        // compiler error. It also allocated twice per file for three fixed
        // strings.
        let route_name = match route {
            Route::Native => "native",
            Route::Scanned => "scanned",
            Route::Flag => "flag",
        };
        let _ = self
            .ledger
            .update_fields(&sha, &[("route", Some(route_name.to_string()))]);

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

        // The transformed SLM input is only trustworthy when the exact
        // selection trace is durable beside the cached source Markdown. The
        // ledger deliberately receives metrics only, because its audit trail
        // is long-lived and must not become a second store of document text.
        if let Err(error) = write_evidence_trace(&self.cfg.cache_dir, &sha, &ev.trace) {
            log::error!("could not persist evidence trace for {sha}: {error:#}");
            self.flag(
                &sha,
                &path,
                "TRACE_WRITE_FAILED:evidence trace could not be saved".into(),
                &clock,
            )
            .await;
            return;
        }
        if let Err(error) =
            self.ledger
                .log_event(&sha, "evidence", &evidence_metric_detail(&ev.trace))
        {
            // The source-bearing trace is already durable, so a transient
            // ledger event failure must not discard an otherwise reviewable
            // document. The full failure remains in the application log.
            log::warn!("could not record evidence metrics for {sha}: {error}");
        }

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
        // Evidence is the document's own embedded metadata. The file's mtime and
        // ctime are deliberately **not** in here.
        //
        // They used to be, and it let the laziest possible answer through: a
        // model that ignores "do NOT use today's date" and proposes today was
        // validated against the file's own timestamp, because a file that just
        // landed in the watched folder was modified today. The name shipped with
        // `date_source: "metadata"` — true, and useless. Measured on a
        // 40-document run before this changed: 25 of 29 completed documents were
        // named with the day of the run rather than the date on the page, several
        // of them documents whose own description quoted the real date correctly.
        //
        // It is circular as well as wrong. `filter.rs` only ever shows the model
        // `doc_meta_dates` (see `assemble_bundle`'s FILE METADATA DATES section),
        // so the model never sees the mtime and cannot be reading it — a match is
        // always coincidence, never evidence. And the mtime is the *fallback*
        // source, reached below through `modified_iso`, so treating it as
        // evidence let it validate a guess and pre-empt the honest fallback that
        // would have flagged it.
        //
        // A document whose real date lives only in its embedded properties is
        // unaffected: those are in `doc_meta_dates`, are shown to the model, and
        // still validate. `README.md`'s guarantee is unchanged in substance and
        // now says "embedded metadata" where it used to say "the file's
        // metadata".
        // `dedup` only collapses *adjacent* equals, which was doing nothing
        // useful when two unrelated sources were concatenated here. It is worth
        // keeping now that there is one source: convertd reports `created` and
        // `modified` next to each other and they are usually the same date.
        let (_fs_dates, modified_iso) = fs_metadata_dates(&path);
        let mut meta_dates = filtered.doc_meta_dates;
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
            Ok(v) => v,
            Err(last_code) => {
                // Keep the documented prefix — `docs/TROUBLESHOOTING.md` lists it
                // and Flow 2 matches on it — and append which rule actually
                // refused. Without that, every naming failure in the index reads
                // identically whether the model invented a date, wrote a subject
                // of nine words, or never answered at all, and the only way to
                // tell them apart was to decrypt the ledger.
                let reason = match last_code {
                    Some(code) => {
                        format!("SLM_FAIL:no valid output after escalation ({code})")
                    }
                    None => "SLM_FAIL:no valid output after escalation".to_string(),
                };
                self.flag(&sha, &path, reason, &clock).await;
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
        let mut m = Manifest {
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
        let emitted = match self.delivery_for_job(&sha) {
            Ok((mode, root)) if mode == "power_automate" => {
                write_manifest(&root.join("_manifests"), &m).map(|_| ())
            }
            Ok((mode, root)) if mode == "local" => {
                self.source_root_for_job(&sha).and_then(|source_root| {
                    self.deliver_local_with_collisions(
                        &sha,
                        &path,
                        &source_root,
                        &validated.base_name,
                        &ext,
                        &mut m,
                        &root,
                    )
                })
            }
            Ok((other, _)) => Err(anyhow::anyhow!(
                "unsupported immutable delivery mode {other}"
            )),
            Err(error) => Err(error),
        };
        match emitted {
            Ok(()) => {
                let _ = self
                    .ledger
                    .update_fields(&sha, &[("recovery_previous_filename", None)]);
                let _ = self.advance(&sha, JobState::Emitted);
                let _ = self.ledger.log_event(&sha, "emit", "delivery committed");
                self.purge_cache(&sha);
            }
            Err(e) => {
                // Local delivery failures deliberately leave source, intent and
                // staging in place for receipt-driven recovery; flagging would
                // move that source and destroy the transaction's authority.
                if self
                    .delivery_for_job(&sha)
                    .ok()
                    .is_some_and(|(mode, _)| mode == "local")
                {
                    log::error!("local delivery pending for {sha}: {e}");
                    let _ = self
                        .ledger
                        .log_event(&sha, "emit", "local delivery pending");
                    return;
                }
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
        for attempt in 1..=self.cfg.max_stage_attempts.max(1) {
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
                        // Also to the app log. A scanner producing systematically
                        // weak OCR costs the naming lane extra attempts, which is
                        // the throughput bottleneck — and the pattern was
                        // invisible across a batch, because this path ends in a
                        // document that ships fine and so nothing else mentions
                        // it anywhere outside the encrypted ledger.
                        log::info!(
                            "convert attempt {attempt}: ocr confidence {:.2} below floor, escalating",
                            c.ocr_mean_conf
                        );
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
            _ => (
                Tier::Escalation,
                filter::widened_bundle(ev, budget.saturating_mul(2)),
            ),
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
        // `Err` carries the code of the last rule that rejected, so the flag
        // reason can name it. `None` inside the `Err` means the ladder never got
        // a parseable proposal at all — a model or transport failure rather than
        // a validation one.
    ) -> Result<crate::checker::Validated, Option<&'static str>> {
        let mut violation: Option<String> = None;
        let mut last_code: Option<&'static str> = None;
        // No classifier ran means no type to declare; saying so beats naming a
        // type the sidecar never actually decided on.
        let doc_type_hint = ev.doc_type.as_deref().unwrap_or("unknown");
        for attempt in 1..=self.cfg.max_stage_attempts.max(1) {
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
                            // Same reasoning as the OCR escalation above: this
                            // path accepts the document either way, so the
                            // ledger was the only place a persistent
                            // Ettin/model disagreement showed up at all.
                            log::info!("name attempt {attempt}: span mismatch, re-prompting");
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
                                        return Ok(v2);
                                    }
                                }
                            }
                            v.soft_flags.push("SPAN_MISMATCH_PERSISTED".into());
                        }
                        let _ = self.advance(sha, JobState::Named);
                        return Ok(v);
                    }
                    Err(ce) => {
                        // Full message (with the offending text) drives the
                        // on-device re-prompt; the persisted log gets the code.
                        violation = Some(ce.to_string());
                        last_code = Some(ce.code());
                        // The code alone, to the app log as well as the ledger.
                        // Rejections used to go only to the ledger, which is
                        // encrypted — so an operator looking at a third of a
                        // backfill sitting in Needs Review had no way to learn
                        // whether it was the subject rule or the date rule
                        // without decrypting a database. The code carries no
                        // document text, which is why it is the part that is safe
                        // to put here (see `CheckError::code`).
                        log::warn!("name attempt {attempt} rejected: {}", ce.code());
                        let _ = self.ledger.log_event(
                            sha,
                            "validate",
                            &format!("attempt {attempt} rejected: {}", ce.code()),
                        );
                        if matches!(ce, CheckError::TooLong(_, _)) {
                            // Length problems rarely improve with escalation;
                            // ask for a shorter subject explicitly, and from the
                            // FIRST rejection — the old `attempt >= 2` guard meant
                            // a TooLong on attempt 1 escalated to the 1.7B without
                            // the primary ever hearing the length-specific ask.
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
        Err(last_code)
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
            // A duplicate of a flagged or dismissed original has nothing sane
            // to emit — but the physical copy must not rot invisibly in
            // Processing. Every zero-byte file shares one sha, so "the second
            // empty file" was exactly this path: no count, no flag, no row.
            // Park the copy in quarantine beside its original so the operator
            // sees one story and Processing stays clean.
            let rel = relpath(&self.cfg.processing_dir, path);
            let dup_key = manifest_id(sha, &rel);
            let _ = std::fs::create_dir_all(&self.cfg.quarantine_dir);
            if crate::watcher::is_safe_path_under_root(&self.cfg.processing_dir, path)
                && path.is_file()
            {
                let dest = self.quarantine_dest(&dup_key, path);
                let moved = std::fs::rename(path, &dest).is_ok()
                    || match copy_then_remove(path, &dest) {
                        Ok(()) => true,
                        Err(e) => {
                            log::error!("failed to quarantine a duplicate of a reviewed file: {e}");
                            let _ =
                                self.ledger
                                    .log_event(sha, "ingest", "DUPLICATE_QUARANTINE_FAILED");
                            false
                        }
                    };
                if moved {
                    log::warn!("quarantined a same-content copy of a file already under review");
                    let _ = self.ledger.log_event(
                        sha,
                        "ingest",
                        "duplicate of a reviewed file quarantined",
                    );
                }
            }
            return;
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
                match self.record_duplicate(&dup_key, sha, path, name, &rel, ext, stem, &orig_final)
                {
                    Ok(fname) => fname,
                    Err(e) => {
                        log::error!("duplicate ledger record failed for {sha}: {e}");
                        return;
                    }
                }
            }
        };

        clock.parked(self.pacer.permit()).await;
        let collision_base = final_filename
            .rsplit_once('.')
            .map(|(stem, _)| stem.to_string())
            .unwrap_or_else(|| final_filename.clone());
        let mut m = Manifest {
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
        let source_root = match self.source_root_for_job(&dup_key) {
            Ok(root) => root,
            Err(error) => {
                log::error!("duplicate delivery source lookup failed for {dup_key}: {error}");
                return;
            }
        };
        // The duplicate does not run naming/classification again, but its
        // Local intent still needs an authoritative ledger snapshot to bind on
        // restart. Persist exactly the inherited proposal before delivery, so
        // intent JSON cannot rewrite any of these values after a crash.
        if let Err(error) = self.ledger.update_fields(
            &dup_key,
            &[
                ("proposed_date", existing.proposed_date.clone()),
                ("date_source", existing.date_source.clone()),
                ("description", existing.description.clone()),
                ("doc_type", existing.doc_type.clone()),
                ("language", existing.language.clone()),
                ("model_versions", Some(self.model_versions.to_string())),
                ("soft_flags", Some("DUPLICATE_CONTENT".into())),
            ],
        ) {
            log::error!("duplicate metadata contract failed for {dup_key}: {error}");
            return;
        }
        match self.delivery_for_job(&dup_key) {
            Ok((mode, root)) if mode == "power_automate" => {
                match write_manifest(&root.join("_manifests"), &m) {
                    Ok(_) => {
                        let _ = self.ledger.set_state(&dup_key, JobState::Emitted);
                        let _ = self
                            .ledger
                            .log_event(sha, "emit", "duplicate manifest written");
                    }
                    Err(e) => {
                        log::error!("duplicate manifest write failed for {dup_key}: {e}");
                        let _ =
                            self.ledger
                                .log_event(sha, "emit", "duplicate manifest write FAILED");
                    }
                }
            }
            Ok((mode, root)) if mode == "local" => match self.deliver_local_with_collisions(
                &dup_key,
                path,
                &source_root,
                &collision_base,
                ext,
                &mut m,
                &root,
            ) {
                Ok(()) => {
                    let _ = self
                        .ledger
                        .update_fields(&dup_key, &[("recovery_previous_filename", None)]);
                    let _ = self.ledger.set_state(&dup_key, JobState::Emitted);
                    let _ = self
                        .ledger
                        .log_event(sha, "emit", "duplicate local receipt written");
                }
                Err(error) => {
                    log::error!("duplicate local delivery pending for {dup_key}: {error}");
                    let _ = self
                        .ledger
                        .log_event(sha, "emit", "duplicate local delivery pending");
                }
            },
            Ok((mode, _)) => log::error!("unsupported duplicate delivery mode {mode}"),
            Err(e) => log::error!("duplicate delivery lookup failed for {dup_key}: {e}"),
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
        content_sha: &str,
        path: &Path,
        name: &str,
        rel: &str,
        ext: &str,
        stem: &str,
        orig_final: &str,
    ) -> anyhow::Result<String> {
        let (mode, root) = self.configured_delivery();
        self.ledger.ingest_with_delivery(
            dup_key,
            &path.to_string_lossy(),
            name,
            rel,
            ext,
            mode,
            &root,
            &self.cfg.processing_dir.to_string_lossy(),
            // The physical duplicate id is the ledger key, not its content
            // hash. Pin both so receipt recovery can validate the artifact.
            content_sha,
        )?;
        let final_filename = self.ledger.reserve_name(stem, ext, dup_key)?;
        self.ledger.update_fields(
            dup_key,
            &[
                ("duplicate_of", Some(orig_final.to_string())),
                ("soft_flags", Some("DUPLICATE_CONTENT".into())),
            ],
        )?;
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

    fn delivery_for_job(&self, sha: &str) -> anyhow::Result<(String, PathBuf)> {
        let mut job = self
            .ledger
            .get(sha)?
            .ok_or_else(|| anyhow::anyhow!("unknown delivery"))?;
        if job.delivery_root.is_empty() && job.delivery_mode == "power_automate" {
            job = self
                .ledger
                .pin_legacy_power_automate_root(sha, &self.cfg.outbox_dir)?
                .ok_or_else(|| anyhow::anyhow!("legacy delivery disappeared while pinning"))?;
        }
        let root = PathBuf::from(&job.delivery_root);
        self.cfg
            .validate_pinned_delivery_root(&job.delivery_mode, &root)
            .map_err(anyhow::Error::msg)?;
        Ok((job.delivery_mode, root))
    }

    /// New rows pin this at intake. The fallback is exclusively for ledgers
    /// written before the column existed.
    fn source_root_for_job(&self, sha: &str) -> anyhow::Result<PathBuf> {
        let job = self
            .ledger
            .get(sha)?
            .ok_or_else(|| anyhow::anyhow!("unknown delivery source"))?;
        Ok(if job.source_root.is_empty() {
            self.cfg.processing_dir.clone()
        } else {
            PathBuf::from(job.source_root)
        })
    }

    #[allow(clippy::too_many_arguments)] // transaction inputs are deliberately explicit
    fn deliver_local_with_collisions(
        &self,
        sha: &str,
        source: &Path,
        source_root: &Path,
        base: &str,
        ext: &str,
        manifest: &mut Manifest,
        root: &Path,
    ) -> anyhow::Result<()> {
        let mut remove_source = |path: &Path| std::fs::remove_file(path);
        self.deliver_local_with_collisions_and_remove(
            sha,
            source,
            source_root,
            base,
            ext,
            manifest,
            root,
            &mut remove_source,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn deliver_local_with_collisions_and_remove(
        &self,
        sha: &str,
        source: &Path,
        source_root: &Path,
        base: &str,
        ext: &str,
        manifest: &mut Manifest,
        root: &Path,
        remove_source: &mut impl FnMut(&Path) -> std::io::Result<()>,
    ) -> anyhow::Result<()> {
        let collision_base = if ext.is_empty() {
            base.to_string()
        } else {
            format!("{base}.{ext}")
        };
        for suffix in 1..=crate::checker::MAX_NAME_COLLISIONS {
            match local_output::deliver_with_remove(
                root,
                source_root,
                source,
                &collision_base,
                manifest,
                &mut *remove_source,
            )? {
                DeliverResult::Delivered => return Ok(()),
                DeliverResult::NameCollision => {
                    let next = suffix.saturating_add(1);
                    let current = manifest
                        .new_filename
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("local collision has no reserved name"))?;
                    let name = self
                        .ledger
                        .advance_local_recovery_name_from(base, ext, sha, current, next)?
                        .ok_or_else(|| anyhow::anyhow!("local collision reservation changed"))?;
                    manifest.new_filename = Some(name.clone());
                }
            }
        }
        anyhow::bail!("local output collision resolution exceeded configured safety limit")
    }

    /// Reconcile the ledger from an already-durable manifest.
    ///
    /// Returns true only when the file's exact content hash, normalized
    /// Processing-relative identity, manifest id, and schema all agree. A
    /// malformed or unrelated JSON file is ignored and normal processing
    /// continues.
    fn recover_terminal_manifest(&self, sha: &str, original_relpath: &str) -> bool {
        let job = match self.ledger.get(sha) {
            Ok(Some(job)) => job,
            Ok(None) => return false,
            Err(error) => {
                log::warn!("could not inspect the ledger for manifest recovery: {error}");
                return false;
            }
        };
        let job = if job.delivery_mode == "power_automate" && job.delivery_root.is_empty() {
            match self
                .ledger
                .pin_legacy_power_automate_root(sha, &self.cfg.outbox_dir)
            {
                Ok(Some(job)) => job,
                Ok(None) => return false,
                Err(error) => {
                    log::warn!("could not pin legacy delivery {sha}: {error}");
                    return false;
                }
            }
        } else {
            job
        };
        let recovery = match job.delivery_mode.as_str() {
            "power_automate" => {
                let root = if job.delivery_root.is_empty() {
                    self.cfg.outbox_dir.clone()
                } else {
                    PathBuf::from(&job.delivery_root)
                };
                match self
                    .cfg
                    .validate_pinned_delivery_root("power_automate", &root)
                {
                    Ok(()) => reconcile_terminal_manifest(
                        &self.cfg,
                        &root.join("_manifests"),
                        &self.ledger,
                        &job,
                        original_relpath,
                    ),
                    Err(error) => Err(anyhow::Error::msg(error)),
                }
            }
            "local" => reconcile_local_receipt(&self.cfg, &self.ledger, &job, original_relpath),
            other => Err(anyhow::anyhow!(
                "unsupported immutable delivery mode {other}"
            )),
        };
        match recovery {
            Ok(Some(target)) => {
                if target == JobState::Emitted {
                    self.purge_cache(sha);
                }
                self.emit_update(sha);
                true
            }
            Ok(None) => false,
            Err(error) => {
                log::warn!("could not reconcile the ledger from a durable manifest: {error}");
                false
            }
        }
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
        // Do not freeze the ledger before the review file and its handoff are
        // durable. The claim is the worker-ownership gate; this state check
        // protects against a late timeout racing an operator decision.
        let existing = match self.ledger.get(sha) {
            Ok(Some(job)) => job,
            Ok(None) => return,
            Err(error) => {
                log::error!("could not inspect {sha} before flagging: {error}");
                return;
            }
        };
        if existing.state.is_resolved() || existing.state == JobState::Flagged {
            log::warn!(
                "not flagging {sha} ({reason}): the job is already resolved or under review"
            );
            return;
        }

        let original_relpath = self.identity_relpath(sha, path);
        let mid = existing.delivery_id.clone();

        // Local review has a durable move plan before the source changes. PA
        // deliberately keeps its established quarantine/manifest sequence.
        if existing.delivery_mode == "local" {
            self.flag_local(sha, path, reason, clock, &existing, &original_relpath, &mid)
                .await;
            return;
        }

        // Move to local quarantine. Never lose the file: move, or copy then
        // remove the source (cross-volume rename fails). Surface a hard failure
        // instead of silently leaving the file orphaned in Processing while the
        // manifest claims it was quarantined.
        let _ = std::fs::create_dir_all(&self.cfg.quarantine_dir);
        let source_is_safe =
            crate::watcher::is_safe_path_under_root(&self.cfg.processing_dir, path);
        let quarantined_path = if source_is_safe && path.is_file() {
            let dest = self.quarantine_dest(&mid, path);
            let moved = std::fs::rename(path, &dest).is_ok()
                || match copy_then_remove(path, &dest) {
                    Ok(()) => true,
                    Err(e) => {
                        log::error!("failed to quarantine a flagged file: {e}");
                        let _ = self.ledger.log_event(sha, "flag", "QUARANTINE_FAILED");
                        false
                    }
                };
            if moved {
                Some(dest)
            } else {
                None
            }
        } else {
            existing
                .quarantine_path
                .as_deref()
                .map(PathBuf::from)
                .filter(|candidate| candidate.is_file())
        };
        let Some(quarantined_path) = quarantined_path else {
            log::error!("flagging could not preserve the review file for {sha}");
            return;
        };
        let quarantined = quarantined_path.to_string_lossy().into_owned();

        // Persist where the file actually went; `resubmit` reads this column
        // rather than reconstructing a name that was never unique.
        if let Err(error) = self.ledger.update_fields(
            sha,
            &[
                ("flag_reason", Some(reason.clone())),
                ("quarantine_path", Some(quarantined.clone())),
                ("quarantine_planned_path", Some(quarantined)),
                (
                    "quarantine_root",
                    Some(self.cfg.quarantine_dir.to_string_lossy().into_owned()),
                ),
            ],
        ) {
            log::error!("could not record quarantine before flagging {sha}: {error}");
            restore_quarantined(&quarantined_path, path);
            return;
        }

        clock.parked(self.pacer.permit()).await;
        let m = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: mid,
            sha256: existing.content_sha256.clone(),
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
            duplicate_of: manifest_duplicate_of(&existing),
            soft_flags: vec![],
            flag_reason: Some(reason.clone()),
            model_versions: self.model_versions.clone(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        let review_written = match self.delivery_for_job(sha) {
            Ok((mode, root)) if mode == "power_automate" => {
                write_manifest(&root.join("_manifests"), &m).map(|_| ())
            }
            Ok((mode, root)) if mode == "local" => {
                local_output::record_review(&root, &self.cfg.quarantine_dir, &quarantined_path, &m)
            }
            Ok((mode, _)) => Err(anyhow::anyhow!(
                "unsupported immutable delivery mode {mode}"
            )),
            Err(error) => Err(error),
        };
        if let Err(error) = review_written {
            log::error!("flagged manifest write failed for {sha}: {error}");
            if restore_quarantined(&quarantined_path, path) {
                let _ = self.ledger.update_fields(
                    sha,
                    &[
                        ("quarantine_path", None),
                        ("quarantine_planned_path", None),
                        ("quarantine_root", None),
                    ],
                );
            } else {
                log::error!("the review file could not be restored after manifest failure");
            }
            return;
        }

        match self.ledger.set_state(sha, JobState::Flagged) {
            Ok(true) => {}
            Ok(false) => {
                // The manifest is the recovery authority. A replay will
                // reconcile this narrow post-write window without rerunning
                // conversion or naming.
                log::warn!("flagged manifest is durable but the ledger moved before commit");
                return;
            }
            Err(error) => {
                log::error!("flagged manifest is durable but ledger commit failed: {error}");
                return;
            }
        }
        let _ = self.ledger.log_event(sha, "flag", &reason);
        // Flagging is a normal review outcome. Keep the reason code visible in
        // support logs without writing document text.
        log::warn!("flagged: {reason}");
        self.emit_update(sha);
    }

    #[allow(clippy::too_many_arguments)]
    async fn flag_local(
        &self,
        sha: &str,
        path: &Path,
        reason: String,
        _clock: &WorkClock,
        job: &Job,
        original_relpath: &str,
        mid: &str,
    ) {
        let quarantine_root = job
            .quarantine_root
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cfg.quarantine_dir.clone());
        let planned = job
            .quarantine_planned_path
            .as_deref()
            .map(PathBuf::from)
            .or_else(|| job.quarantine_path.as_deref().map(PathBuf::from))
            .unwrap_or_else(|| self.quarantine_dest(mid, path));
        if job.quarantine_planned_path.is_none() {
            if let Err(error) = self.ledger.update_fields(
                sha,
                &[
                    ("flag_reason", Some(reason.clone())),
                    (
                        "quarantine_planned_path",
                        Some(planned.to_string_lossy().into_owned()),
                    ),
                    (
                        "quarantine_root",
                        Some(quarantine_root.to_string_lossy().into_owned()),
                    ),
                ],
            ) {
                log::error!("could not persist local quarantine plan for {sha}: {error}");
                return;
            }
        }

        let quarantined_ready = crate::watcher::is_safe_path_under_root(&quarantine_root, &planned)
            && planned.is_file()
            && hash_file(&planned).ok().as_deref() == Some(job.content_sha256.as_str());
        if !quarantined_ready {
            if !crate::watcher::is_safe_path_under_root(&self.cfg.processing_dir, path)
                || hash_file(path).ok().as_deref() != Some(job.content_sha256.as_str())
            {
                log::error!("local quarantine plan for {sha} has no safe matching source");
                return;
            }
            let _ = std::fs::create_dir_all(&quarantine_root);
            if std::fs::rename(path, &planned).is_err() {
                if let Err(error) = copy_then_remove(path, &planned) {
                    log::error!("failed to execute local quarantine plan for {sha}: {error}");
                    return;
                }
            }
        }
        if !crate::watcher::is_safe_path_under_root(&quarantine_root, &planned)
            || hash_file(&planned).ok().as_deref() != Some(job.content_sha256.as_str())
        {
            log::error!("local quarantine plan did not preserve {sha}");
            return;
        }
        if let Err(error) = self.ledger.update_fields(
            sha,
            &[
                ("flag_reason", Some(reason.clone())),
                (
                    "quarantine_path",
                    Some(planned.to_string_lossy().into_owned()),
                ),
            ],
        ) {
            log::error!("could not record local quarantine for {sha}: {error}");
            return;
        }
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: mid.into(),
            sha256: job.content_sha256.clone(),
            status: "flagged".into(),
            original_name: job.original_name.clone(),
            original_relpath: original_relpath.into(),
            new_filename: None,
            description: None,
            date: None,
            date_source: None,
            doc_type: job.doc_type.clone(),
            language: job.language.clone(),
            duplicate_of: manifest_duplicate_of(job),
            soft_flags: vec![],
            flag_reason: Some(reason),
            model_versions: job
                .model_versions
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_else(|| serde_json::json!({})),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        let root = PathBuf::from(&job.delivery_root);
        if let Err(error) = self.cfg.validate_pinned_delivery_root("local", &root) {
            log::error!("refusing local flagged receipt for {sha}: {error}");
            return;
        }
        if let Err(error) =
            local_output::record_review(&root, &quarantine_root, &planned, &manifest)
        {
            log::error!("failed to record local flagged receipt for {sha}: {error}");
            return;
        }
        match self.ledger.set_state(sha, JobState::Flagged) {
            Ok(true) => {
                let _ = self
                    .ledger
                    .log_event(sha, "flag", "local quarantine receipt written");
                self.emit_update(sha);
            }
            Ok(false) => {
                log::warn!("local flagged receipt is durable but ledger CAS lost for {sha}")
            }
            Err(error) => {
                log::error!("local flagged receipt is durable but ledger failed for {sha}: {error}")
            }
        }
    }

    /// Human correction from the review pane: re-validate and re-emit.
    pub async fn resubmit(
        &self,
        sha: &str,
        date: String,
        subject: String,
        description: String,
    ) -> anyhow::Result<()> {
        self.resubmit_with_owner(sha, date, subject, description, None)
            .await
    }

    async fn resubmit_with_owner(
        &self,
        sha: &str,
        date: String,
        subject: String,
        description: String,
        existing_review_owner: Option<String>,
    ) -> anyhow::Result<()> {
        self.resubmit_with_owner_and_remove(
            sha,
            date,
            subject,
            description,
            existing_review_owner,
            |path| std::fs::remove_file(path),
        )
        .await
    }

    async fn resubmit_with_owner_and_remove(
        &self,
        sha: &str,
        date: String,
        subject: String,
        description: String,
        existing_review_owner: Option<String>,
        mut remove_source: impl FnMut(&Path) -> std::io::Result<()>,
    ) -> anyhow::Result<()> {
        let job = self
            .ledger
            .get(sha)?
            .ok_or_else(|| anyhow::anyhow!("unknown job"))?;
        let job = if job.delivery_mode == "power_automate" && job.delivery_root.is_empty() {
            self.ledger
                .pin_legacy_power_automate_root(sha, &self.cfg.outbox_dir)?
                .ok_or_else(|| anyhow::anyhow!("legacy job disappeared while pinning"))?
        } else {
            job
        };
        if job.state != JobState::Flagged {
            anyhow::bail!("Only Flagged jobs in Needs Review may be resubmitted");
        }
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
        let mid = job.delivery_id.clone();
        anyhow::ensure!(
            matches!(job.delivery_mode.as_str(), "local" | "power_automate"),
            "unsupported immutable delivery mode"
        );

        let review_owner = match existing_review_owner {
            Some(owner) => owner,
            None => self
                .ledger
                .begin_review_operation(sha, "correct")?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "That review item is already being decided; refresh and try again"
                    )
                })?,
        };

        // The reservation is a ledger write, so it is the one mutation that
        // must precede the manifest; it is rolled back below if the write
        // fails, leaving the job exactly as the operator found it.
        let previous_final = job.final_filename.clone();
        let final_filename = match self.ledger.reserve_name(&v.base_name, &job.ext, sha) {
            Ok(name) => name,
            Err(error) => {
                let _ = self
                    .ledger
                    .release_review_operation(sha, "correct", &review_owner);
                return Err(error);
            }
        };

        let duplicate_of = manifest_duplicate_of(&job);
        let m = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: mid,
            sha256: job.content_sha256.clone(),
            status: "ok".into(),
            original_name: job.original_name.clone(),
            original_relpath: original_relpath.clone(),
            new_filename: Some(final_filename.clone()),
            description: Some(v.description.clone()),
            date: Some(v.date_iso.clone()),
            date_source: Some("human".into()),
            doc_type: job.doc_type.clone(),
            language: job.language.clone(),
            duplicate_of,
            soft_flags: vec!["HUMAN_CORRECTED".into()],
            flag_reason: None,
            model_versions: self.model_versions.clone(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        if job.delivery_mode == "local" {
            let source = match job.quarantine_path.as_deref().map(PathBuf::from) {
                Some(source) => source,
                None => {
                    let _ = self
                        .ledger
                        .release_review_operation(sha, "correct", &review_owner);
                    anyhow::bail!("local review item has no recorded quarantine path");
                }
            };
            let root = PathBuf::from(&job.delivery_root);
            let quarantine_root = job
                .quarantine_root
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.cfg.quarantine_dir.clone());
            if root.as_os_str().is_empty() {
                let _ = self
                    .ledger
                    .release_review_operation(sha, "correct", &review_owner);
                anyhow::bail!("local review item has no recorded output root");
            }
            if let Err(error) = self.cfg.validate_pinned_delivery_root("local", &root) {
                let _ = self
                    .ledger
                    .release_review_operation(sha, "correct", &review_owner);
                return Err(anyhow::Error::msg(error));
            }
            let mut local_manifest = m.clone();
            if let Err(error) = self.deliver_local_with_collisions_and_remove(
                sha,
                &source,
                &quarantine_root,
                &v.base_name,
                &job.ext,
                &mut local_manifest,
                &root,
                &mut remove_source,
            ) {
                let collision_base = if job.ext.is_empty() {
                    v.base_name.clone()
                } else {
                    format!("{}.{}", v.base_name, job.ext)
                };
                let durable = match local_output::durable_transaction_exists(
                    &root,
                    &quarantine_root,
                    &source,
                    &collision_base,
                    &local_manifest,
                ) {
                    Ok(durable) => durable,
                    Err(inspect_error) => {
                        log::warn!(
                            "could not inspect failed Local correction transaction; preserving it for recovery: {inspect_error}"
                        );
                        true
                    }
                };
                if !durable {
                    let _ = self.ledger.update_fields(
                        sha,
                        &[
                            ("final_filename", previous_final),
                            ("recovery_previous_filename", None),
                        ],
                    );
                    let _ = self
                        .ledger
                        .release_review_operation(sha, "correct", &review_owner);
                }
                return Err(error);
            }
            if !self.ledger.commit_live_correction(
                sha,
                &review_owner,
                &[
                    ("final_filename", local_manifest.new_filename.clone()),
                    ("recovery_previous_filename", None),
                    ("proposed_date", Some(v.date_iso)),
                    ("date_source", Some("human".into())),
                    ("proposed_subject", Some(v.subject)),
                    ("description", Some(v.description)),
                    ("doc_type", local_manifest.doc_type.clone()),
                    ("language", local_manifest.language.clone()),
                    (
                        "model_versions",
                        Some(local_manifest.model_versions.to_string()),
                    ),
                    ("flag_reason", None),
                    ("quarantine_path", None),
                    ("quarantine_planned_path", None),
                    ("quarantine_root", None),
                    ("soft_flags", Some("HUMAN_CORRECTED".into())),
                ],
            )? {
                anyhow::bail!("job {sha} is no longer flagged; local delivery remains recoverable");
            }
            self.ledger
                .log_event(sha, "resubmit", "local human correction accepted")?;
            self.purge_cache(sha);
            self.emit_update(sha);
            return Ok(());
        }
        // Nothing else commits until the manifest is on disk. The old order
        // cleared flag_reason, logged the correction and un-quarantined the
        // document first, so a failed write left a flagged row with no reason,
        // a pending flagged manifest, and the file back in Processing where
        // the watcher re-ingested it as a spurious duplicate.
        let pa_root = if job.delivery_root.is_empty() {
            self.cfg.outbox_dir.clone()
        } else {
            PathBuf::from(&job.delivery_root)
        };
        if let Err(error) = self
            .cfg
            .validate_pinned_delivery_root("power_automate", &pa_root)
        {
            let _ = self
                .ledger
                .release_review_operation(sha, "correct", &review_owner);
            return Err(anyhow::Error::msg(error));
        }
        if let Err(e) = write_manifest(&pa_root.join("_manifests"), &m) {
            let _ = self
                .ledger
                .update_fields(sha, &[("final_filename", previous_final)]);
            let _ = self.ledger.log_event(
                sha,
                "resubmit",
                "correction rejected: manifest write failed",
            );
            let _ = self
                .ledger
                .release_review_operation(sha, "correct", &review_owner);
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
        // Keep the exact quarantine locator until the source is back under its
        // intake-time Processing root. The manifest must precede the restore
        // for durable recovery, but success is never returned until Flow 2 can
        // see both that manifest and the restored source.
        if let Err(error) = restore_power_automate_correction(&self.cfg, &job, &original_relpath) {
            let _ = self.ledger.log_event(
                sha,
                "resubmit",
                "RESTORE_FAILED: correction manifest remains recoverable",
            );
            return Err(error);
        }

        // The exact owner-token CAS is deliberately after restoration: a
        // crash after the move leaves a flagged row plus an `ok` manifest,
        // which startup can verify and terminalize idempotently. It is the
        // only operation that clears the restoration locator.
        if !self.ledger.commit_live_correction(
            sha,
            &review_owner,
            &[
                ("final_filename", m.new_filename.clone()),
                ("recovery_previous_filename", None),
                ("proposed_date", Some(v.date_iso)),
                ("date_source", Some("human".into())),
                ("proposed_subject", Some(v.subject)),
                ("description", Some(v.description)),
                ("doc_type", m.doc_type.clone()),
                ("language", m.language.clone()),
                ("model_versions", Some(m.model_versions.to_string())),
                ("flag_reason", None),
                ("quarantine_path", None),
                ("quarantine_planned_path", None),
                ("quarantine_root", None),
                ("soft_flags", Some("HUMAN_CORRECTED".into())),
            ],
        )? {
            let _ = self.ledger.log_event(
                sha,
                "resubmit",
                "correction abandoned: the job is no longer flagged",
            );
            anyhow::bail!(
                "job {sha} is no longer flagged; correction manifest remains recoverable"
            );
        }
        self.ledger
            .log_event(sha, "resubmit", "human correction accepted")?;

        // Quarantined original moves back into scope for Flow 2's rename — to
        // the relative location its identity was computed from, not to the
        // Processing root under its leaf name.
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
        purge_cache_artifacts(&self.cfg.cache_dir, sha);
    }
}

fn evidence_trace_path(cache_dir: &Path, sha: &str) -> PathBuf {
    cache_dir.join(format!("{sha}.evidence.json"))
}

/// Persist the reversible evidence-selection trace without ever exposing it to
/// a partially written final path. The trace contains exact source text, so it
/// follows the same retention lifecycle as the cached Markdown.
fn write_evidence_trace(
    cache_dir: &Path,
    sha: &str,
    trace: &filter::EvidenceTrace,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(cache_dir)?;

    let final_path = evidence_trace_path(cache_dir, sha);
    let backup_path = cache_dir.join(format!("{sha}.evidence.json.bak"));
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_path = cache_dir.join(format!(
        ".{sha}.evidence.{}.{}.tmp",
        std::process::id(),
        nonce
    ));

    let mut encoded = serde_json::to_vec_pretty(trace)?;
    encoded.push(b'\n');
    let write_result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(&encoded)?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }

    // Windows rename does not replace an existing file. Move the previous
    // complete trace aside first, then restore it if the final rename fails.
    let had_previous = final_path.exists();
    if had_previous {
        let _ = std::fs::remove_file(&backup_path);
        if let Err(error) = std::fs::rename(&final_path, &backup_path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error.into());
        }
    }

    if let Err(error) = std::fs::rename(&temp_path, &final_path) {
        if had_previous {
            let _ = std::fs::rename(&backup_path, &final_path);
        }
        let _ = std::fs::remove_file(&temp_path);
        return Err(error.into());
    }
    if had_previous {
        let _ = std::fs::remove_file(&backup_path);
    }
    Ok(final_path)
}

/// Source-free summary suitable for the encrypted ledger. Exact paragraphs,
/// entities, names, and document identifiers remain only in the cache trace.
fn evidence_metric_detail(trace: &filter::EvidenceTrace) -> String {
    let compression = &trace.compression;
    let savings_permille = if compression.source_chars == 0 {
        0
    } else {
        compression
            .saved_chars
            .saturating_mul(1_000)
            .saturating_div(compression.source_chars)
    };
    format!(
        "routing={};source_chars={};bundle_chars={};saved_chars={};\
         savings_permille={};paragraphs={}/{};semantic={};entities={}",
        trace.routing,
        compression.source_chars,
        compression.bundle_chars,
        compression.saved_chars,
        savings_permille,
        trace.selected_paragraphs,
        trace.source_paragraphs,
        trace.semantic_available,
        trace.entity_available,
    )
}

fn purge_cache_artifacts(cache_dir: &Path, sha: &str) {
    for path in [
        cache_dir.join(format!("{sha}.md")),
        evidence_trace_path(cache_dir, sha),
        cache_dir.join(format!("{sha}.evidence.json.bak")),
    ] {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "could not remove cache artifact {}: {error}",
                    path.display()
                );
            }
        }
    }
}

fn cache_artifact_sha(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    name.strip_suffix(".evidence.json")
        .or_else(|| name.strip_suffix(".md"))
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

fn same_path_identity(left: &Path, right: &Path) -> bool {
    crate::identity::normalize_relpath(&left.to_string_lossy())
        == crate::identity::normalize_relpath(&right.to_string_lossy())
}

fn validate_local_receipt_identity(
    receipt: &local_output::Receipt,
    job: &Job,
    original_relpath: &str,
) -> anyhow::Result<()> {
    let output_name_matches_ledger = if receipt.manifest.status == "ok" {
        let receipt_name = receipt
            .manifest
            .new_filename
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("local ok receipt has no output name"))?;
        receipt.output_relpath.as_deref() == Some(receipt_name)
            && (job.final_filename.as_deref() == Some(receipt_name)
                || job.recovery_previous_filename.as_deref() == Some(receipt_name))
    } else {
        true
    };
    anyhow::ensure!(
        receipt.receipt_schema == 1
            && receipt.delivery_mode == "local"
            && receipt.manifest.validate().is_ok()
            && receipt.manifest.manifest_id == job.delivery_id
            && receipt.manifest.sha256 == job.content_sha256
            && receipt.manifest.duplicate_of == manifest_duplicate_of(job)
            && crate::identity::normalize_relpath(&receipt.manifest.original_relpath)
                == crate::identity::normalize_relpath(original_relpath)
            && output_name_matches_ledger,
        "local receipt does not match its ledger delivery"
    );
    Ok(())
}

fn validate_local_review_receipt(
    receipt: &local_output::Receipt,
    job: &Job,
    quarantine_root: &Path,
    planned: Option<&Path>,
) -> anyhow::Result<()> {
    if !matches!(receipt.manifest.status.as_str(), "flagged" | "dismissed") {
        return Ok(());
    }
    anyhow::ensure!(
        receipt.output_relpath.is_none() && receipt.manifest.new_filename.is_none(),
        "local review receipt cannot name an output"
    );
    let quarantined =
        planned.ok_or_else(|| anyhow::anyhow!("local review receipt has no pinned source"))?;
    let receipt_source_root = PathBuf::from(&receipt.source_root);
    let receipt_source = PathBuf::from(&receipt.source_path);
    anyhow::ensure!(
        same_path_identity(&receipt_source_root, quarantine_root)
            && same_path_identity(&receipt_source, quarantined),
        "local review receipt source provenance does not match pinned quarantine"
    );
    anyhow::ensure!(
        crate::watcher::is_safe_path_under_root(quarantine_root, &receipt_source)
            && receipt_source.is_file(),
        "local review receipt source is missing or outside pinned quarantine"
    );
    anyhow::ensure!(
        hash_file(&receipt_source)? == job.content_sha256,
        "local review receipt source content does not match ledger"
    );
    Ok(())
}

/// Reconcile every unresolved row whose exact terminal manifest is already
/// durable. This runs before the watcher starts, so a source moved to
/// quarantine immediately before a crash does not need to reappear in
/// Processing to finish its ledger transition.
pub fn reconcile_terminal_manifests(cfg: &Config, ledger: &Ledger) -> anyhow::Result<usize> {
    let mut recovered = 0usize;
    for job in ledger.unresolved_jobs()? {
        let attempted_delivery_id = job.delivery_id.clone();
        // A poisoned receipt, an unavailable old root, or one row's SQLite/IO
        // fault must never prevent later rows from reconciling. Keep every
        // potentially-mutating action inside the row transaction, and only
        // release an abandoned review lease after that transaction succeeded
        // with no terminal artifact. On an error we leave the row untouched.
        let outcome = (|| -> anyhow::Result<(Job, Option<JobState>)> {
            let job = if job.delivery_mode == "power_automate" && job.delivery_root.is_empty() {
                ledger
                    .pin_legacy_power_automate_root(&job.sha256, &cfg.outbox_dir)?
                    .ok_or_else(|| anyhow::anyhow!("legacy delivery disappeared while pinning"))?
            } else {
                job
            };
            let job = pin_legacy_pa_review_roots(cfg, ledger, job)?;
            let original_relpath = job.original_relpath.clone().unwrap_or_else(|| {
                relpath(&cfg.processing_dir, Path::new(job.original_path.as_str()))
            });
            let target = match job.delivery_mode.as_str() {
                "power_automate" => {
                    let root = if job.delivery_root.is_empty() {
                        cfg.outbox_dir.clone()
                    } else {
                        PathBuf::from(&job.delivery_root)
                    };
                    cfg.validate_pinned_delivery_root("power_automate", &root)
                        .map_err(anyhow::Error::msg)?;
                    reconcile_terminal_manifest(
                        cfg,
                        &root.join("_manifests"),
                        ledger,
                        &job,
                        &original_relpath,
                    )?
                }
                "local" => reconcile_local_receipt(cfg, ledger, &job, &original_relpath)?,
                other => {
                    anyhow::bail!("unsupported immutable delivery mode {other}");
                }
            };
            Ok((job, target))
        })();

        let (job, target) = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                log::warn!(
                    "terminal reconciliation skipped delivery {} after a row-local error: {error}",
                    attempted_delivery_id
                );
                continue;
            }
        };
        if target.is_none() {
            if let Some(operation) = job.review_operation.as_deref() {
                let _ = ledger.release_abandoned_review_operation(&job.sha256, operation);
            }
        }
        if let Some(target) = target {
            if target == JobState::Emitted && !cfg.retain_cache {
                purge_cache_artifacts(&cfg.cache_dir, &job.sha256);
            }
            recovered += 1;
        }
    }
    Ok(recovered)
}

fn pin_legacy_pa_review_roots(cfg: &Config, ledger: &Ledger, job: Job) -> anyhow::Result<Job> {
    if job.delivery_mode != "power_automate"
        || job.state != JobState::Flagged
        || (job.quarantine_root.is_some() && !job.source_root.is_empty())
    {
        return Ok(job);
    }
    let Some(quarantined) = job.quarantine_path.as_deref().map(PathBuf::from) else {
        return Ok(job);
    };
    anyhow::ensure!(
        crate::watcher::is_safe_path_under_root(&cfg.quarantine_dir, &quarantined)
            && quarantined.is_file()
            && hash_file(&quarantined)? == job.content_sha256,
        "legacy PA review source cannot be safely pinned"
    );
    let original_relpath = job
        .original_relpath
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("legacy PA review row has no original relpath"))?;
    let expected_original = cfg.processing_dir.join(original_relpath);
    anyhow::ensure!(
        same_path_identity(&expected_original, Path::new(&job.original_path)),
        "legacy PA review source root does not match the recorded intake path"
    );
    ledger
        .pin_legacy_power_automate_review_roots(
            &job.sha256,
            &cfg.processing_dir,
            &cfg.quarantine_dir,
            &quarantined,
        )?
        .ok_or_else(|| anyhow::anyhow!("legacy PA review row disappeared while pinning"))
}

fn reconcile_local_receipt(
    cfg: &Config,
    ledger: &Ledger,
    job: &Job,
    original_relpath: &str,
) -> anyhow::Result<Option<JobState>> {
    let root = PathBuf::from(&job.delivery_root);
    anyhow::ensure!(
        !root.as_os_str().is_empty(),
        "local job has no pinned output root"
    );
    cfg.validate_pinned_delivery_root("local", &root)
        .map_err(anyhow::Error::msg)?;
    // This is deliberately not the ledger key: an ordinary legacy row is
    // keyed by its content hash, while a physical duplicate row is keyed by
    // this value. Both persist the exact receipt identity at ingest.
    let mid = job.delivery_id.clone();
    let quarantine_root = job
        .quarantine_root
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.quarantine_dir.clone());
    let planned = job
        .quarantine_path
        .as_deref()
        .or(job.quarantine_planned_path.as_deref())
        .map(PathBuf::from);
    // Validate persisted transaction provenance against ledger-owned paths
    // before recovery reads or deletes anything. Intent JSON is durable but
    // not an authority to redirect a job outside its pinned source contract.
    let ordinary_root = if job.source_root.is_empty() {
        cfg.processing_dir.clone()
    } else {
        PathBuf::from(&job.source_root)
    };
    let ordinary_source = PathBuf::from(&job.original_path);
    let (expected_root, expected_source) = match planned.as_ref() {
        Some(quarantined) => (&quarantine_root, quarantined),
        None => (&ordinary_root, &ordinary_source),
    };
    // A pending correction intent may coexist with its pre-review flagged
    // receipt. Validate that receipt completely before intent recovery can
    // publish or delete anything; corrupt review metadata is never authority
    // to touch the pinned source or release its active decision lease.
    if let Some(persisted_receipt) = local_output::read_receipt(&root, &mid)? {
        validate_local_receipt_identity(&persisted_receipt, job, original_relpath)?;
        validate_local_review_receipt(
            &persisted_receipt,
            job,
            &quarantine_root,
            planned.as_deref(),
        )?;
    }
    if local_output::validate_intent_for_recovery(
        &root,
        job,
        original_relpath,
        expected_root,
        expected_source,
    )? {
        ensure_recovery_operation_compatible(job, JobState::Emitted, "local intent")?;
        let current_name = job
            .final_filename
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("local intent has no pinned output name"))?;
        let _ = local_output::recover_intent_with_name_sync(
            &root,
            &job.delivery_id,
            current_name,
            |expected, candidate| {
                ledger.advance_local_recovery_filename(&job.sha256, expected, candidate)
            },
        )?;
    }
    // Intent recovery may atomically advance the ledger through one or more
    // filesystem collisions. Bind the resulting receipt and terminal CAS to
    // that current reservation rather than the row snapshot from before the
    // recovery loop.
    let refreshed_job = ledger
        .get(&job.sha256)?
        .ok_or_else(|| anyhow::anyhow!("local delivery disappeared during recovery"))?;
    let job = &refreshed_job;
    let receipt = match local_output::read_receipt(&root, &mid)? {
        Some(receipt) => receipt,
        None => {
            // Planned move survived a crash before the receipt. It is safe to
            // complete only when the exact pinned quarantine file exists and
            // still hashes to this delivery.
            let Some(quarantined) = planned.as_ref() else {
                return Ok(None);
            };
            if !crate::watcher::is_safe_path_under_root(&quarantine_root, quarantined)
                || hash_file(quarantined).ok().as_deref() != Some(job.content_sha256.as_str())
            {
                return Ok(None);
            }
            let manifest = Manifest {
                schema: MANIFEST_SCHEMA_VERSION,
                manifest_id: mid.clone(),
                sha256: job.content_sha256.clone(),
                status: "flagged".into(),
                original_name: job.original_name.clone(),
                original_relpath: original_relpath.into(),
                new_filename: None,
                description: None,
                date: None,
                date_source: None,
                doc_type: job.doc_type.clone(),
                language: job.language.clone(),
                duplicate_of: manifest_duplicate_of(job),
                soft_flags: vec![],
                flag_reason: job
                    .flag_reason
                    .clone()
                    .or_else(|| Some("RECOVERED:planned quarantine".into())),
                model_versions: job
                    .model_versions
                    .as_deref()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .unwrap_or_else(|| serde_json::json!({})),
                processed_at: chrono::Utc::now().to_rfc3339(),
            };
            local_output::record_review(&root, &quarantine_root, quarantined, &manifest)?;
            local_output::read_receipt(&root, &mid)?.ok_or_else(|| {
                anyhow::anyhow!("local flagged receipt disappeared during recovery")
            })?
        }
    };
    let receipt_source_root = PathBuf::from(&receipt.source_root);
    let receipt_source = PathBuf::from(&receipt.source_path);
    validate_local_receipt_identity(&receipt, job, original_relpath)?;
    validate_local_review_receipt(&receipt, job, &quarantine_root, planned.as_deref())?;
    let manifest = receipt.manifest;
    let target = match manifest.status.as_str() {
        "ok" => {
            if !local_output::receipt_is_complete(&root, &manifest)? {
                return Ok(None);
            }
            let expected_correction = planned
                .as_ref()
                .map(|source| *source == receipt_source && quarantine_root == receipt_source_root);
            let original_root = if job.source_root.is_empty() {
                cfg.processing_dir.clone()
            } else {
                PathBuf::from(&job.source_root)
            };
            let expected_ordinary = receipt_source == Path::new(&job.original_path)
                && receipt_source_root == original_root;
            anyhow::ensure!(
                expected_correction == Some(true) || (planned.is_none() && expected_ordinary),
                "local receipt source provenance does not match its pinned ledger source"
            );
            JobState::Emitted
        }
        "flagged" | "dismissed" => {
            if manifest.status == "flagged" {
                JobState::Flagged
            } else {
                JobState::Dismissed
            }
        }
        _ => return Ok(None),
    };
    if job.state == target {
        // A flagged receipt is the pre-review artifact, not a competing
        // terminal decision. With no state/field transition to perform, its
        // only safe startup action is releasing an owner abandoned before a
        // correction or dismissal became durable.
        if target == JobState::Flagged {
            if let Some(operation) = job.review_operation.as_deref() {
                let _ = ledger.release_abandoned_review_operation(&job.sha256, operation);
            }
        }
        return Ok(None);
    }
    ensure_recovery_operation_compatible(job, target, "local receipt")?;
    if target == JobState::Emitted && receipt_source.exists() {
        // Resume the exact Processing or quarantine transaction that produced
        // this receipt only after the terminal artifact is proven compatible
        // with the active review lease.
        local_output::deliver(&root, &receipt_source_root, &receipt_source, &manifest)?;
    }
    let fields: Vec<(&str, Option<String>)> = match target {
        JobState::Emitted => vec![
            ("final_filename", manifest.new_filename.clone()),
            ("recovery_previous_filename", None),
            ("proposed_date", manifest.date.clone()),
            ("date_source", manifest.date_source.clone()),
            ("description", manifest.description.clone()),
            ("doc_type", manifest.doc_type.clone()),
            ("language", manifest.language.clone()),
            ("soft_flags", Some(manifest.soft_flags.join(","))),
            ("model_versions", Some(manifest.model_versions.to_string())),
            ("quarantine_path", None),
            ("quarantine_planned_path", None),
            ("quarantine_root", None),
        ],
        JobState::Flagged | JobState::Dismissed => vec![
            ("flag_reason", manifest.flag_reason.clone()),
            (
                "quarantine_path",
                planned.map(|path| path.to_string_lossy().into_owned()),
            ),
            ("model_versions", Some(manifest.model_versions.to_string())),
        ],
        _ => unreachable!(),
    };
    let committed = ledger.commit_recovered_terminal(
        &job.sha256,
        job.state,
        job.review_operation.as_deref(),
        target,
        &fields,
    )?;
    if !committed {
        return Ok(None);
    }
    let _ = ledger.log_event(&job.sha256, "recover", "reconciled durable local receipt");
    Ok(Some(target))
}

fn reconcile_terminal_manifest(
    cfg: &Config,
    manifests_dir: &Path,
    ledger: &Ledger,
    job: &Job,
    original_relpath: &str,
) -> anyhow::Result<Option<JobState>> {
    let mid = job.delivery_id.clone();
    let path = manifests_dir.join(format!("{mid}.json"));
    let manifest: Manifest = match std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(manifest) => manifest,
        None => return Ok(None),
    };
    if manifest.validate().is_err()
        || manifest.manifest_id != mid
        || manifest.sha256 != job.content_sha256
        || manifest.duplicate_of != manifest_duplicate_of(job)
        || crate::identity::normalize_relpath(&manifest.original_relpath)
            != crate::identity::normalize_relpath(original_relpath)
    {
        log::warn!("ignored a manifest that did not match its ledger delivery");
        return Ok(None);
    }

    let target = match manifest.status.as_str() {
        "ok" => JobState::Emitted,
        "flagged" => JobState::Flagged,
        "dismissed" => JobState::Dismissed,
        _ => return Ok(None),
    };
    if job.state == target {
        if target == JobState::Flagged {
            if let Some(operation) = job.review_operation.as_deref() {
                let _ = ledger.release_abandoned_review_operation(&job.sha256, operation);
            }
        }
        return Ok(None);
    }
    ensure_recovery_operation_compatible(job, target, "Power Automate manifest")?;
    if target == JobState::Emitted
        && job.state == JobState::Flagged
        && job.review_operation.as_deref() == Some("correct")
    {
        restore_power_automate_correction(cfg, job, original_relpath)?;
    }
    let fields: Vec<(&str, Option<String>)> = match target {
        JobState::Emitted => vec![
            ("final_filename", manifest.new_filename.clone()),
            ("proposed_date", manifest.date.clone()),
            ("date_source", manifest.date_source.clone()),
            ("description", manifest.description.clone()),
            ("doc_type", manifest.doc_type.clone()),
            ("language", manifest.language.clone()),
            ("soft_flags", Some(manifest.soft_flags.join(","))),
            ("model_versions", Some(manifest.model_versions.to_string())),
            ("flag_reason", None),
            ("quarantine_path", None),
            ("quarantine_planned_path", None),
            ("quarantine_root", None),
        ],
        JobState::Flagged | JobState::Dismissed => vec![
            ("flag_reason", manifest.flag_reason.clone()),
            ("model_versions", Some(manifest.model_versions.to_string())),
        ],
        _ => unreachable!("a manifest always maps to a terminal state"),
    };
    let committed = ledger.commit_recovered_terminal(
        &job.sha256,
        job.state,
        job.review_operation.as_deref(),
        target,
        &fields,
    )?;
    if !committed {
        return Ok(None);
    }
    let _ = ledger.log_event(&job.sha256, "recover", "reconciled durable manifest");
    Ok(Some(target))
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
    let artifact_shas: HashSet<String> = entries
        .flatten()
        .filter_map(|entry| cache_artifact_sha(&entry.path()).map(str::to_owned))
        .collect();

    for sha in artifact_shas {
        // Anything the ledger still has an unresolved row for is evidence a
        // human is (or will be) looking at. An error reading the ledger fails
        // closed the same way.
        match ledger.job_state(&sha) {
            Ok(Some(state)) if !state.is_resolved() => continue,
            Err(error) => {
                log::warn!("cache sweep skipping {sha}: ledger read failed: {error}");
                continue;
            }
            _ => {}
        }

        let artifacts = [
            cache_dir.join(format!("{sha}.md")),
            evidence_trace_path(cache_dir, &sha),
        ];
        let any_fresh = artifacts.iter().any(|path| {
            path.metadata()
                .and_then(|metadata| metadata.modified())
                .map(|modified| modified >= cutoff)
                .unwrap_or(false)
        });
        if !any_fresh {
            purge_cache_artifacts(cache_dir, &sha);
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

/// Move a PA correction's review copy back to the immutable Processing path.
///
/// The manifest is already durable when this runs, so it treats both a source
/// still in Quarantine and a matching destination with no source as valid
/// restart states. A destination that appeared while the source remains is
/// never adopted or replaced, even if its bytes happen to match.
fn restore_power_automate_correction(
    cfg: &Config,
    job: &Job,
    original_relpath: &str,
) -> anyhow::Result<()> {
    restore_power_automate_correction_with(cfg, job, original_relpath, |source, destination| {
        match std::fs::rename(source, destination) {
            Ok(()) => Ok(()),
            Err(rename_error) => copy_then_remove(source, destination).map_err(|copy_error| {
                std::io::Error::new(
                    copy_error.kind(),
                    format!("rename failed: {rename_error}; no-replace copy fallback failed: {copy_error}"),
                )
            }),
        }
    })
}

fn restore_power_automate_correction_with(
    cfg: &Config,
    job: &Job,
    original_relpath: &str,
    move_source: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        job.delivery_mode == "power_automate" && job.state == JobState::Flagged,
        "PA restoration is only valid for a flagged correction"
    );
    let processing_root = if job.source_root.is_empty() {
        cfg.processing_dir.clone()
    } else {
        PathBuf::from(&job.source_root)
    };
    anyhow::ensure!(
        !processing_root.as_os_str().is_empty(),
        "PA correction has no pinned Processing root"
    );
    anyhow::ensure!(
        crate::identity::normalize_relpath(original_relpath)
            == crate::identity::normalize_relpath(
                job.original_relpath.as_deref().unwrap_or(original_relpath)
            ),
        "PA correction restoration relpath does not match its ledger identity"
    );
    anyhow::ensure!(
        crate::watcher::is_safe_path_under_root(&processing_root, &processing_root)
            && Path::new(original_relpath)
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "PA correction pinned Processing root or relative destination is unsafe"
    );
    let destination = processing_root.join(original_relpath);
    if !job.source_root.is_empty() {
        anyhow::ensure!(
            same_path_identity(&destination, Path::new(&job.original_path)),
            "PA correction destination does not match the intake-time source path"
        );
    }
    let quarantine_root = job
        .quarantine_root
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("PA correction has no pinned quarantine root"))?;
    let source = job
        .quarantine_path
        .as_deref()
        .or(job.quarantine_planned_path.as_deref())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("PA correction has no pinned quarantine source"))?;
    anyhow::ensure!(
        crate::watcher::is_safe_path_under_root(&quarantine_root, &quarantine_root)
            && source.strip_prefix(&quarantine_root).is_ok()
            && (!source.exists()
                || crate::watcher::is_safe_path_under_root(&quarantine_root, &source)),
        "PA correction source is outside pinned Quarantine or unsafe"
    );

    if destination.exists() {
        anyhow::ensure!(
            crate::watcher::is_safe_path_under_root(&processing_root, &destination)
                && destination.is_file()
                && hash_file(&destination)? == job.content_sha256,
            "PA correction destination is foreign or has different content"
        );
        anyhow::ensure!(
            !source.exists(),
            "PA correction destination already exists; preserving pinned quarantine source"
        );
        return Ok(());
    }
    anyhow::ensure!(
        source.is_file() && hash_file(&source)? == job.content_sha256,
        "PA correction pinned quarantine source is missing or changed"
    );
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    anyhow::ensure!(
        destination
            .parent()
            .is_some_and(|parent| crate::watcher::is_safe_path_under_root(
                &processing_root,
                parent
            ))
            && !destination.exists(),
        "PA correction destination became unsafe or occupied during restore"
    );
    move_source(&source, &destination)?;
    anyhow::ensure!(
        !source.exists()
            && crate::watcher::is_safe_path_under_root(&processing_root, &destination)
            && destination.is_file()
            && hash_file(&destination)? == job.content_sha256,
        "PA correction restore did not durably move the pinned source"
    );
    Ok(())
}

fn copy_then_remove(source: &Path, destination: &Path) -> std::io::Result<()> {
    copy_then_remove_with(source, destination, |path| std::fs::remove_file(path))
}

fn copy_then_remove_with(
    source: &Path,
    destination: &Path,
    remove_source: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    // `std::fs::copy` truncates an existing destination. Quarantine names are
    // collision-resistant but a race with another process is still possible,
    // so the cross-volume fallback must retain create-new semantics too.
    let mut input = std::fs::File::open(source)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    if let Err(error) = remove_source(source) {
        if let Err(cleanup_error) = std::fs::remove_file(destination) {
            log::error!(
                "failed to remove an incomplete quarantine copy after source deletion failed: \
                 {cleanup_error}"
            );
        }
        return Err(error);
    }
    Ok(())
}

/// Roll a quarantined file back into Processing after a pre-terminal failure.
///
/// Refuses to overwrite a file that appeared at the original path while the
/// worker was writing its manifest. In that rare case the quarantined copy is
/// retained and the error is logged rather than destroying either version.
fn restore_quarantined(quarantined: &Path, original: &Path) -> bool {
    if original.exists() || !quarantined.is_file() {
        return false;
    }
    if let Some(parent) = original.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    if std::fs::rename(quarantined, original).is_ok() {
        return true;
    }
    match std::fs::copy(quarantined, original) {
        Ok(_) => {
            if std::fs::remove_file(quarantined).is_ok() {
                true
            } else {
                let _ = std::fs::remove_file(original);
                false
            }
        }
        Err(_) => false,
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

    #[tokio::test]
    async fn resubmit_refuses_a_non_flagged_job_before_writing_a_manifest() {
        let h = Harness::new();
        let (sha, _path) = h.seed("not-review.pdf", "a converted document body");

        let error = h
            .pipeline
            .resubmit(
                &sha,
                "2024-03-05".into(),
                "Invoice - Acme".into(),
                "A valid human correction sentence.".into(),
            )
            .await
            .expect_err("only flagged jobs may be resubmitted");
        assert!(error.to_string().contains("Flagged"), "{error:#}");
        assert!(h.manifest(&sha, "not-review.pdf").is_none());
        assert_eq!(
            h.pipeline.ledger.get(&sha).unwrap().unwrap().state,
            JobState::Ingested
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
                &cfg.local_output_dir,
                &cfg.quarantine_dir,
                &cfg.cache_dir,
            ] {
                if !d.as_os_str().is_empty() {
                    std::fs::create_dir_all(d).unwrap();
                }
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
                2,
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

        /// Settings are applied only while stopped. Recreate the runtime over
        /// the same ledger to model that restart without changing a job's
        /// immutable delivery contract.
        fn restarted_with(&self, cfg: Config) -> Arc<Pipeline> {
            Arc::new(Pipeline {
                convert_slots: self.pipeline.convert_slots.clone(),
                slm_slots: self.pipeline.slm_slots.clone(),
                ingest_slots: self.pipeline.ingest_slots.clone(),
                inflight: self.pipeline.inflight.clone(),
                pacer: self.pipeline.pacer.clone(),
                cfg,
                ledger: self.pipeline.ledger.clone(),
                sidecar: self.pipeline.sidecar.clone(),
                slm: self.pipeline.slm.clone(),
                app: None,
                paused: self.pipeline.paused.clone(),
                model_versions: self.pipeline.model_versions.clone(),
            })
        }

        fn app_state(&self) -> crate::AppState {
            crate::AppState {
                cfg_path: self.dir.path().join("backlog.config.json"),
                cfg: std::sync::Mutex::new(self.pipeline.cfg.clone()),
                default_cache_dir: self.pipeline.cfg.cache_dir.clone(),
                log_path: self.dir.path().join("backlog.log"),
                ledger: self.pipeline.ledger.clone(),
                pipeline: std::sync::Mutex::new(None),
                last_preflight: std::sync::Mutex::new(None),
            }
        }

        /// Create a document at `rel` under Processing and give it an ingested
        /// ledger row, exactly as `process_inner` would have.
        fn seed(&self, rel: &str, body: &str) -> (String, PathBuf) {
            let path = self.pipeline.cfg.processing_dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            let sha = hash_file(&path).unwrap();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let (mode, root) = self.pipeline.configured_delivery();
            self.pipeline
                .ledger
                .ingest_with_delivery(
                    &sha,
                    &path.to_string_lossy(),
                    &name,
                    rel,
                    "pdf",
                    mode,
                    &root,
                    &self.pipeline.cfg.processing_dir.to_string_lossy(),
                    &sha,
                )
                .unwrap();
            (sha, path)
        }

        /// Create the ledger shape used for a physical/per-path duplicate:
        /// the row key is its delivery id, while receipt identity and content
        /// hash remain separate immutable fields.
        fn seed_duplicate(&self, rel: &str, body: &str) -> (String, String, PathBuf) {
            let path = self.pipeline.cfg.processing_dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            let content_sha = hash_file(&path).unwrap();
            let delivery_id = manifest_id(&content_sha, rel);
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let (mode, root) = self.pipeline.configured_delivery();
            self.pipeline
                .ledger
                .ingest_with_delivery(
                    &delivery_id,
                    &path.to_string_lossy(),
                    &name,
                    rel,
                    "pdf",
                    mode,
                    &root,
                    &self.pipeline.cfg.processing_dir.to_string_lossy(),
                    &content_sha,
                )
                .unwrap();
            // The ledger's display-oriented duplicate field historically
            // stores the first reserved filename. Manifest duplicate_of must
            // still use the true content hash rather than copying this value.
            self.pipeline
                .ledger
                .update_fields(
                    &delivery_id,
                    &[("duplicate_of", Some("already-filed.pdf".into()))],
                )
                .unwrap();
            (delivery_id, content_sha, path)
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

    #[test]
    fn local_planned_quarantine_recovers_after_settings_switch_without_pa_manifest() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let source = h.pipeline.cfg.processing_dir.join("nested/scan.pdf");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"planned quarantine").unwrap();
        let sha = hash_file(&source).unwrap();
        let rel = "nested/scan.pdf";
        let name = "scan.pdf";
        h.pipeline
            .ledger
            .ingest_with_delivery(
                &sha,
                &source.to_string_lossy(),
                name,
                rel,
                "pdf",
                "local",
                &h.pipeline.cfg.local_output_dir.to_string_lossy(),
                &h.pipeline.cfg.processing_dir.to_string_lossy(),
                &sha,
            )
            .unwrap();
        let mid = manifest_id(&sha, rel);
        let planned = h.pipeline.quarantine_dest(&mid, &source);
        std::fs::rename(&source, &planned).unwrap();
        h.pipeline
            .ledger
            .update_fields(
                &sha,
                &[
                    ("flag_reason", Some("TEST:planned".into())),
                    (
                        "quarantine_planned_path",
                        Some(planned.to_string_lossy().into_owned()),
                    ),
                    (
                        "quarantine_root",
                        Some(h.pipeline.cfg.quarantine_dir.to_string_lossy().into_owned()),
                    ),
                ],
            )
            .unwrap();

        let mut switched = h.pipeline.cfg.clone();
        switched.output_mode = crate::config::OutputMode::PowerAutomate;
        switched.outbox_dir = h.dir.path().join("different-pa-outbox");
        assert_eq!(
            reconcile_terminal_manifests(&switched, &h.pipeline.ledger).unwrap(),
            1
        );
        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(job.state, JobState::Flagged);
        assert_eq!(job.quarantine_path.as_deref(), planned.to_str());
        assert!(
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &mid)
                .unwrap()
                .is_some()
        );
        assert!(!switched
            .manifests_dir()
            .join(format!("{mid}.json"))
            .exists());
    }

    #[test]
    fn corrupt_first_local_intent_does_not_block_later_quarantine_recovery() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });

        // This row sorts first. Its durable intent is poisoned, so recovery
        // must fail closed for this row alone without preventing the later
        // planned-quarantine transaction from becoming reviewable.
        let poison_source = h.pipeline.cfg.processing_dir.join("a/poison.pdf");
        std::fs::create_dir_all(poison_source.parent().unwrap()).unwrap();
        std::fs::write(&poison_source, b"poisoned intent bytes").unwrap();
        let poison_sha = hash_file(&poison_source).unwrap();
        h.pipeline
            .ledger
            .ingest_with_delivery(
                &poison_sha,
                &poison_source.to_string_lossy(),
                "poison.pdf",
                "a/poison.pdf",
                "pdf",
                "local",
                &h.pipeline.cfg.local_output_dir.to_string_lossy(),
                &h.pipeline.cfg.processing_dir.to_string_lossy(),
                &poison_sha,
            )
            .unwrap();
        let poison_job = h.pipeline.ledger.get(&poison_sha).unwrap().unwrap();
        let poison_delivery = poison_job.delivery_id.clone();
        let intents = h.pipeline.cfg.local_output_dir.join(".backlog/intents");
        std::fs::create_dir_all(&intents).unwrap();
        let poisoned_manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: poison_delivery.clone(),
            sha256: poison_sha.clone(),
            status: "ok".into(),
            original_name: poison_job.original_name.clone(),
            // Structurally valid JSON and Manifest, but not this delivery's
            // ledger-owned path. Recovery must reject it before publication.
            original_relpath: "other/poison.pdf".into(),
            new_filename: Some("2026-08-04 Poison.pdf".into()),
            description: Some("Tampered but otherwise valid intent.".into()),
            date: Some("2026-08-04".into()),
            date_source: Some("document".into()),
            doc_type: Some("document".into()),
            language: Some("en".into()),
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: None,
            model_versions: json!({"convertd": "test"}),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        let poisoned_receipt = local_output::Receipt {
            receipt_schema: 1,
            delivery_mode: "local".into(),
            output_relpath: poisoned_manifest.new_filename.clone(),
            source_root: h.pipeline.cfg.processing_dir.to_string_lossy().into_owned(),
            source_path: poison_source.to_string_lossy().into_owned(),
            manifest: poisoned_manifest,
        };
        let mut poisoned_intent = serde_json::to_value(poisoned_receipt).unwrap();
        let object = poisoned_intent.as_object_mut().unwrap();
        object.insert("intent_schema".into(), json!(1));
        object.insert("output_base_name".into(), json!("2026-08-04 Poison.pdf"));
        let poisoned_intent = serde_json::to_vec(&poisoned_intent).unwrap();
        let poisoned_intent_path = intents.join(format!("{poison_delivery}.json"));
        std::fs::write(&poisoned_intent_path, &poisoned_intent).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let valid_source = h.pipeline.cfg.processing_dir.join("z/valid.pdf");
        std::fs::create_dir_all(valid_source.parent().unwrap()).unwrap();
        std::fs::write(&valid_source, b"later planned quarantine bytes").unwrap();
        let valid_sha = hash_file(&valid_source).unwrap();
        h.pipeline
            .ledger
            .ingest_with_delivery(
                &valid_sha,
                &valid_source.to_string_lossy(),
                "valid.pdf",
                "z/valid.pdf",
                "pdf",
                "local",
                &h.pipeline.cfg.local_output_dir.to_string_lossy(),
                &h.pipeline.cfg.processing_dir.to_string_lossy(),
                &valid_sha,
            )
            .unwrap();
        let valid_delivery = h
            .pipeline
            .ledger
            .get(&valid_sha)
            .unwrap()
            .unwrap()
            .delivery_id;
        let planned = h.pipeline.quarantine_dest(&valid_delivery, &valid_source);
        std::fs::rename(&valid_source, &planned).unwrap();
        h.pipeline
            .ledger
            .update_fields(
                &valid_sha,
                &[
                    ("flag_reason", Some("TEST:planned quarantine".into())),
                    (
                        "quarantine_planned_path",
                        Some(planned.to_string_lossy().into_owned()),
                    ),
                    (
                        "quarantine_root",
                        Some(h.pipeline.cfg.quarantine_dir.to_string_lossy().into_owned()),
                    ),
                ],
            )
            .unwrap();

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            1
        );
        assert_eq!(
            h.pipeline.ledger.get(&poison_sha).unwrap().unwrap().state,
            JobState::Ingested,
            "the poisoned row remains untouched for a safe later retry"
        );
        assert!(
            poison_source.exists(),
            "invalid intent must not delete its source"
        );
        assert!(
            !h.pipeline
                .cfg
                .local_output_dir
                .join("2026-08-04 Poison.pdf")
                .exists(),
            "invalid intent must not publish an output"
        );
        assert!(
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &poison_delivery)
                .unwrap()
                .is_none(),
            "invalid intent must not create a receipt"
        );
        assert_eq!(
            std::fs::read(&poisoned_intent_path).unwrap(),
            poisoned_intent,
            "invalid intent is retained byte-for-byte for a later safe diagnosis"
        );
        let recovered = h.pipeline.ledger.get(&valid_sha).unwrap().unwrap();
        assert_eq!(recovered.state, JobState::Flagged);
        assert_eq!(recovered.quarantine_path.as_deref(), planned.to_str());
        assert!(planned.exists());
    }

    #[test]
    fn local_intent_cannot_change_the_reserved_output_filename() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "ordinary/pinned-name.pdf";
        let source = h.pipeline.cfg.processing_dir.join(rel);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"pinned output intent bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        h.pipeline
            .ledger
            .ingest_with_delivery(
                &sha,
                &source.to_string_lossy(),
                "pinned-name.pdf",
                rel,
                "pdf",
                "local",
                &h.pipeline.cfg.local_output_dir.to_string_lossy(),
                &h.pipeline.cfg.processing_dir.to_string_lossy(),
                &sha,
            )
            .unwrap();
        h.pipeline
            .ledger
            .update_fields(
                &sha,
                &[
                    ("final_filename", Some("2026-08-04 Pinned Name.pdf".into())),
                    ("proposed_date", Some("2026-08-04".into())),
                    ("date_source", Some("document".into())),
                    (
                        "description",
                        Some("A document with a ledger-pinned output name.".into()),
                    ),
                    ("doc_type", Some("document".into())),
                    ("language", Some("en".into())),
                    ("soft_flags", Some("SOURCE_CONFIRMED".into())),
                    (
                        "model_versions",
                        Some(json!({"convertd": "test"}).to_string()),
                    ),
                ],
            )
            .unwrap();
        let before = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: before.delivery_id.clone(),
            sha256: before.content_sha256.clone(),
            status: "ok".into(),
            original_name: before.original_name.clone(),
            original_relpath: rel.into(),
            // Every other field is the exact persisted contract. Only this
            // filename (and the matching receipt/base pair below) is tampered.
            new_filename: Some("2026-08-04 Pinned Name (2).pdf".into()),
            description: before.description.clone(),
            date: before.proposed_date.clone(),
            date_source: before.date_source.clone(),
            doc_type: before.doc_type.clone(),
            language: before.language.clone(),
            duplicate_of: None,
            soft_flags: vec!["SOURCE_CONFIRMED".into()],
            flag_reason: None,
            model_versions: serde_json::from_str(before.model_versions.as_deref().unwrap())
                .unwrap(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        let receipt = local_output::Receipt {
            receipt_schema: 1,
            delivery_mode: "local".into(),
            output_relpath: manifest.new_filename.clone(),
            source_root: h.pipeline.cfg.processing_dir.to_string_lossy().into_owned(),
            source_path: source.to_string_lossy().into_owned(),
            manifest,
        };
        let mut intent = serde_json::to_value(receipt).unwrap();
        let object = intent.as_object_mut().unwrap();
        object.insert("intent_schema".into(), json!(1));
        object.insert(
            "output_base_name".into(),
            json!("2026-08-04 Pinned Name.pdf"),
        );
        let intent = serde_json::to_vec(&intent).unwrap();
        let intent_path = h
            .pipeline
            .cfg
            .local_output_dir
            .join(".backlog/intents")
            .join(format!("{}.json", before.delivery_id));
        std::fs::create_dir_all(intent_path.parent().unwrap()).unwrap();
        std::fs::write(&intent_path, &intent).unwrap();

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0
        );
        let after = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert!(source.exists());
        assert!(!h
            .pipeline
            .cfg
            .local_output_dir
            .join("2026-08-04 Pinned Name (2).pdf")
            .exists());
        assert!(
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &before.delivery_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(intent_path).unwrap(), intent);
    }

    #[test]
    fn local_collision_recovery_resumes_after_ledger_then_intent_crash() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "ordinary/collision-restart.pdf";
        let source = h.pipeline.cfg.processing_dir.join(rel);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"ledger-bound local collision recovery").unwrap();
        let sha = hash_file(&source).unwrap();
        let initial = "2026-08-04 Collision Restart.pdf";
        h.pipeline
            .ledger
            .ingest_with_delivery(
                &sha,
                &source.to_string_lossy(),
                "collision-restart.pdf",
                rel,
                "pdf",
                "local",
                &h.pipeline.cfg.local_output_dir.to_string_lossy(),
                &h.pipeline.cfg.processing_dir.to_string_lossy(),
                &sha,
            )
            .unwrap();
        h.pipeline
            .ledger
            .update_fields(
                &sha,
                &[
                    ("final_filename", Some(initial.into())),
                    ("proposed_date", Some("2026-08-04".into())),
                    ("date_source", Some("document".into())),
                    (
                        "description",
                        Some("A document that must resume its collision recovery.".into()),
                    ),
                    ("doc_type", Some("document".into())),
                    ("language", Some("en".into())),
                    ("soft_flags", Some("SOURCE_CONFIRMED".into())),
                    (
                        "model_versions",
                        Some(json!({"convertd": "test"}).to_string()),
                    ),
                ],
            )
            .unwrap();
        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: job.delivery_id.clone(),
            sha256: job.content_sha256.clone(),
            status: "ok".into(),
            original_name: job.original_name.clone(),
            original_relpath: rel.into(),
            new_filename: Some(initial.into()),
            description: job.description.clone(),
            date: job.proposed_date.clone(),
            date_source: job.date_source.clone(),
            doc_type: job.doc_type.clone(),
            language: job.language.clone(),
            duplicate_of: None,
            soft_flags: vec!["SOURCE_CONFIRMED".into()],
            flag_reason: None,
            model_versions: serde_json::from_str(job.model_versions.as_deref().unwrap()).unwrap(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        let receipt = local_output::Receipt {
            receipt_schema: 1,
            delivery_mode: "local".into(),
            output_relpath: manifest.new_filename.clone(),
            source_root: h.pipeline.cfg.processing_dir.to_string_lossy().into_owned(),
            source_path: source.to_string_lossy().into_owned(),
            manifest,
        };
        let mut intent = serde_json::to_value(receipt).unwrap();
        let object = intent.as_object_mut().unwrap();
        object.insert("intent_schema".into(), json!(1));
        object.insert("output_base_name".into(), json!(initial));
        let intent_path = h
            .pipeline
            .cfg
            .local_output_dir
            .join(".backlog/intents")
            .join(format!("{}.json", job.delivery_id));
        std::fs::create_dir_all(intent_path.parent().unwrap()).unwrap();
        std::fs::write(&intent_path, serde_json::to_vec(&intent).unwrap()).unwrap();
        std::fs::write(
            h.pipeline.cfg.local_output_dir.join(initial),
            b"someone else's file",
        )
        .unwrap();

        let injected = local_output::recover_intent_with_name_sync_for_test(
            &h.pipeline.cfg.local_output_dir,
            &job.delivery_id,
            initial,
            |expected, candidate| {
                h.pipeline
                    .ledger
                    .advance_local_recovery_filename(&sha, expected, candidate)
            },
            || {
                Err(anyhow::anyhow!(
                    "injected crash after durable intent rewrite"
                ))
            },
        );
        assert!(injected.is_err());
        let after_crash = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let recovered_name = "2026-08-04 Collision Restart (2).pdf";
        assert_eq!(after_crash.final_filename.as_deref(), Some(recovered_name));
        assert_eq!(
            after_crash.recovery_previous_filename.as_deref(),
            Some(initial)
        );
        assert!(source.exists());

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            1
        );
        let complete = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(complete.state, JobState::Emitted);
        assert_eq!(complete.final_filename.as_deref(), Some(recovered_name));
        assert!(complete.recovery_previous_filename.is_none());
        assert!(!source.exists());
        assert_eq!(
            hash_file(&h.pipeline.cfg.local_output_dir.join(recovered_name)).unwrap(),
            sha
        );
    }

    #[test]
    fn live_local_collision_recovers_when_ledger_advances_before_intent_for_ordinary_and_duplicate()
    {
        for duplicate in [false, true] {
            let h = Harness::with(|cfg| {
                cfg.output_mode = crate::config::OutputMode::Local;
                cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
            });
            let kind = if duplicate { "duplicate" } else { "ordinary" };
            let rel = format!("live-boundary/{kind}.pdf");
            let source = h.pipeline.cfg.processing_dir.join(&rel);
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            let body = format!("live ledger-before-intent recovery for {kind}");
            std::fs::write(&source, body.as_bytes()).unwrap();
            let content_sha = hash_file(&source).unwrap();
            let row_key = if duplicate {
                manifest_id(&content_sha, &rel)
            } else {
                content_sha.clone()
            };
            let base = format!("2026-08-04 Live {kind}");
            let initial = format!("{base}.pdf");
            let recovered_name = format!("{base} (2).pdf");
            let safe_name = format!("{base} (3).pdf");
            let soft_flags = if duplicate {
                vec!["DUPLICATE_CONTENT".to_string()]
            } else {
                vec!["SOURCE_CONFIRMED".to_string()]
            };

            h.pipeline
                .ledger
                .ingest_with_delivery(
                    &row_key,
                    &source.to_string_lossy(),
                    &format!("{kind}.pdf"),
                    &rel,
                    "pdf",
                    "local",
                    &h.pipeline.cfg.local_output_dir.to_string_lossy(),
                    &h.pipeline.cfg.processing_dir.to_string_lossy(),
                    &content_sha,
                )
                .unwrap();
            h.pipeline
                .ledger
                .update_fields(
                    &row_key,
                    &[
                        ("final_filename", Some(initial.clone())),
                        ("proposed_date", Some("2026-08-04".into())),
                        ("date_source", Some("document".into())),
                        (
                            "description",
                            Some(format!(
                                "A {kind} document crossing the live collision gap."
                            )),
                        ),
                        ("doc_type", Some("document".into())),
                        ("language", Some("en".into())),
                        ("soft_flags", Some(soft_flags.join(","))),
                        (
                            "model_versions",
                            Some(json!({"convertd": "test"}).to_string()),
                        ),
                    ],
                )
                .unwrap();
            let job = h.pipeline.ledger.get(&row_key).unwrap().unwrap();
            let manifest = Manifest {
                schema: MANIFEST_SCHEMA_VERSION,
                manifest_id: job.delivery_id.clone(),
                sha256: content_sha.clone(),
                status: "ok".into(),
                original_name: job.original_name.clone(),
                original_relpath: rel.clone(),
                new_filename: Some(initial.clone()),
                description: job.description.clone(),
                date: job.proposed_date.clone(),
                date_source: job.date_source.clone(),
                doc_type: job.doc_type.clone(),
                language: job.language.clone(),
                duplicate_of: duplicate.then_some(content_sha.clone()),
                soft_flags,
                flag_reason: None,
                model_versions: serde_json::from_str(job.model_versions.as_deref().unwrap())
                    .unwrap(),
                processed_at: chrono::Utc::now().to_rfc3339(),
            };
            std::fs::create_dir_all(&h.pipeline.cfg.local_output_dir).unwrap();
            std::fs::write(
                h.pipeline.cfg.local_output_dir.join(&initial),
                b"unrelated operator file",
            )
            .unwrap();

            assert_eq!(
                local_output::deliver_with_collision_base(
                    &h.pipeline.cfg.local_output_dir,
                    &h.pipeline.cfg.processing_dir,
                    &source,
                    &initial,
                    &manifest,
                )
                .unwrap(),
                DeliverResult::NameCollision
            );
            assert_eq!(
                h.pipeline
                    .ledger
                    .advance_local_recovery_name_from(&base, "pdf", &row_key, &initial, 2)
                    .unwrap()
                    .as_deref(),
                Some(recovered_name.as_str())
            );

            // This is the exact live crash gap: the ledger moved, while the
            // already-durable intent still names the collided predecessor.
            let intent_path = h
                .pipeline
                .cfg
                .local_output_dir
                .join(".backlog/intents")
                .join(format!("{}.json", job.delivery_id));
            let intent: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&intent_path).unwrap()).unwrap();
            assert_eq!(intent["new_filename"].as_str(), Some(initial.as_str()));
            let after_advance = h.pipeline.ledger.get(&row_key).unwrap().unwrap();
            assert_eq!(
                after_advance.final_filename.as_deref(),
                Some(recovered_name.as_str())
            );
            assert_eq!(
                after_advance.recovery_previous_filename.as_deref(),
                Some(initial.as_str())
            );
            std::fs::write(
                h.pipeline.cfg.local_output_dir.join(&recovered_name),
                body.as_bytes(),
            )
            .unwrap();

            assert_eq!(
                reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
                1
            );
            let recovered = h.pipeline.ledger.get(&row_key).unwrap().unwrap();
            assert_eq!(recovered.state, JobState::Emitted);
            assert_eq!(
                recovered.final_filename.as_deref(),
                Some(safe_name.as_str())
            );
            assert!(recovered.recovery_previous_filename.is_none());
            assert!(!source.exists());
            assert_eq!(
                std::fs::read(h.pipeline.cfg.local_output_dir.join(&initial)).unwrap(),
                b"unrelated operator file"
            );
            assert_eq!(
                std::fs::read(h.pipeline.cfg.local_output_dir.join(&recovered_name)).unwrap(),
                body.as_bytes(),
                "same-hash foreign suffix must not be adopted"
            );
            assert_eq!(
                std::fs::read(h.pipeline.cfg.local_output_dir.join(&safe_name)).unwrap(),
                body.as_bytes()
            );
            let receipt =
                local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &job.delivery_id)
                    .unwrap()
                    .unwrap();
            assert_eq!(
                receipt.manifest.duplicate_of,
                duplicate.then_some(content_sha)
            );
        }
    }

    #[test]
    fn local_duplicate_receipt_recovery_uses_pinned_content_and_processing_root() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let old_processing = h.pipeline.cfg.processing_dir.clone();
        let source = old_processing.join("copy.pdf");
        std::fs::write(&source, b"same physical duplicate").unwrap();
        let content_sha = hash_file(&source).unwrap();
        let rel = "copy.pdf";
        let duplicate_id = manifest_id(&content_sha, rel);
        h.pipeline
            .ledger
            .ingest_with_delivery(
                &duplicate_id,
                &source.to_string_lossy(),
                "copy.pdf",
                rel,
                "pdf",
                "local",
                &h.pipeline.cfg.local_output_dir.to_string_lossy(),
                &old_processing.to_string_lossy(),
                &content_sha,
            )
            .unwrap();
        h.pipeline
            .ledger
            .update_fields(
                &duplicate_id,
                &[("final_filename", Some("copy (2).pdf".into()))],
            )
            .unwrap();
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: duplicate_id.clone(),
            sha256: content_sha.clone(),
            status: "ok".into(),
            original_name: "copy.pdf".into(),
            original_relpath: rel.into(),
            new_filename: Some("copy (2).pdf".into()),
            description: Some("duplicate receipt interruption".into()),
            date: Some("2026-08-04".into()),
            date_source: Some("document".into()),
            doc_type: Some("document".into()),
            language: Some("en".into()),
            duplicate_of: Some(content_sha.clone()),
            soft_flags: vec!["DUPLICATE_CONTENT".into()],
            flag_reason: None,
            model_versions: json!({ "convertd": "test" }),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        // Simulate death after durable receipt/output but before the final
        // source deletion and ledger state transition.
        local_output::deliver(
            &h.pipeline.cfg.local_output_dir,
            &old_processing,
            &source,
            &manifest,
        )
        .unwrap();
        std::fs::write(&source, b"same physical duplicate").unwrap();
        assert_eq!(
            h.pipeline.ledger.get(&duplicate_id).unwrap().unwrap().state,
            JobState::Ingested
        );

        let mut switched = h.pipeline.cfg.clone();
        switched.processing_dir = h.dir.path().join("different-processing");
        std::fs::create_dir_all(&switched.processing_dir).unwrap();
        assert_eq!(
            reconcile_terminal_manifests(&switched, &h.pipeline.ledger).unwrap(),
            1
        );
        let job = h.pipeline.ledger.get(&duplicate_id).unwrap().unwrap();
        assert_eq!(job.content_sha256, content_sha);
        assert_eq!(job.state, JobState::Emitted);
        assert!(!source.exists());
        assert!(h
            .pipeline
            .cfg
            .local_output_dir
            .join("copy (2).pdf")
            .exists());
    }

    #[tokio::test]
    async fn tampered_flagged_review_output_pair_preserves_row_source_and_lease() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "review/tampered-output.pdf";
        let body = b"flagged review output pair bytes";
        let (sha, source) = h.seed(rel, std::str::from_utf8(body).unwrap());
        h.pipeline
            .flag(&sha, &source, "SLM_FAIL:review".into(), &clock())
            .await;
        let owner = h
            .pipeline
            .ledger
            .begin_review_operation(&sha, "correct")
            .unwrap()
            .unwrap();
        let before = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let quarantined = PathBuf::from(before.quarantine_path.as_ref().unwrap());
        let receipt_path = h
            .pipeline
            .cfg
            .local_output_dir
            .join(".backlog/receipts")
            .join(format!("{}.json", before.delivery_id));
        let mut receipt =
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &before.delivery_id)
                .unwrap()
                .unwrap();

        receipt.output_relpath = Some("must-not-exist.pdf".into());
        std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        let output_relpath_tamper = std::fs::read(&receipt_path).unwrap();
        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0
        );
        assert_eq!(std::fs::read(&receipt_path).unwrap(), output_relpath_tamper);
        assert_eq!(std::fs::read(&quarantined).unwrap(), body);
        assert!(!source.exists());
        assert!(!h
            .pipeline
            .cfg
            .local_output_dir
            .join("must-not-exist.pdf")
            .exists());
        let after_output_tamper = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&after_output_tamper).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(
            after_output_tamper.review_owner.as_deref(),
            Some(owner.as_str())
        );

        receipt.output_relpath = None;
        receipt.manifest.new_filename = Some("also-must-not-exist.pdf".into());
        std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        let manifest_name_tamper = std::fs::read(&receipt_path).unwrap();
        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0
        );
        assert_eq!(std::fs::read(&receipt_path).unwrap(), manifest_name_tamper);
        assert_eq!(std::fs::read(&quarantined).unwrap(), body);
        assert!(!h
            .pipeline
            .cfg
            .local_output_dir
            .join("also-must-not-exist.pdf")
            .exists());
        let after_manifest_tamper = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&after_manifest_tamper).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(
            after_manifest_tamper.review_owner.as_deref(),
            Some(owner.as_str())
        );
    }

    #[tokio::test]
    async fn receipt_only_ok_recovery_rejects_same_hash_foreign_name_outside_ledger_reservation() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "review/tampered-receipt-name.pdf";
        let body = b"receipt name must remain ledger bound";
        let (sha, processing_source) = h.seed(rel, std::str::from_utf8(body).unwrap());
        h.pipeline
            .flag(&sha, &processing_source, "SLM_FAIL:review".into(), &clock())
            .await;
        let flagged = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let quarantine_root = PathBuf::from(flagged.quarantine_root.as_ref().unwrap());
        let quarantined = PathBuf::from(flagged.quarantine_path.as_ref().unwrap());
        let owner = h
            .pipeline
            .ledger
            .begin_review_operation(&sha, "correct")
            .unwrap()
            .unwrap();
        let reserved_name = "2026-08-04 Ledger Reserved.pdf";
        h.pipeline
            .ledger
            .update_fields(&sha, &[("final_filename", Some(reserved_name.into()))])
            .unwrap();
        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let corrected = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: job.delivery_id.clone(),
            sha256: job.content_sha256.clone(),
            status: "ok".into(),
            original_name: job.original_name.clone(),
            original_relpath: rel.into(),
            new_filename: Some(reserved_name.into()),
            description: Some("A ledger-bound corrected document.".into()),
            date: Some("2026-08-04".into()),
            date_source: Some("human".into()),
            doc_type: job.doc_type.clone(),
            language: job.language.clone(),
            duplicate_of: None,
            soft_flags: vec!["HUMAN_CORRECTED".into()],
            flag_reason: None,
            model_versions: json!({"human": "test"}),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        let delete_error = local_output::deliver_with_remove_for_test(
            &h.pipeline.cfg.local_output_dir,
            &quarantine_root,
            &quarantined,
            &corrected,
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected delete failure",
                ))
            },
        )
        .unwrap_err();
        assert!(delete_error.to_string().contains("injected delete failure"));
        let intent_path = h
            .pipeline
            .cfg
            .local_output_dir
            .join(".backlog/intents")
            .join(format!("{}.json", job.delivery_id));
        std::fs::remove_file(intent_path).unwrap();

        let foreign_name = "same-hash-foreign.pdf";
        let foreign = h.pipeline.cfg.local_output_dir.join(foreign_name);
        std::fs::write(&foreign, body).unwrap();
        let receipt_path = h
            .pipeline
            .cfg
            .local_output_dir
            .join(".backlog/receipts")
            .join(format!("{}.json", job.delivery_id));
        let mut receipt: local_output::Receipt =
            serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
        receipt.output_relpath = Some(foreign_name.into());
        receipt.manifest.new_filename = Some(foreign_name.into());
        let tampered = serde_json::to_vec_pretty(&receipt).unwrap();
        std::fs::write(&receipt_path, &tampered).unwrap();
        let before = h.pipeline.ledger.get(&sha).unwrap().unwrap();

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0
        );
        let after = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(after.state, JobState::Flagged);
        assert_eq!(after.review_owner.as_deref(), Some(owner.as_str()));
        assert!(quarantined.exists());
        assert_eq!(std::fs::read(&foreign).unwrap(), body);
        assert_eq!(
            std::fs::read(h.pipeline.cfg.local_output_dir.join(reserved_name)).unwrap(),
            body
        );
        assert_eq!(std::fs::read(receipt_path).unwrap(), tampered);
    }

    #[tokio::test]
    async fn tampered_dismissed_review_source_preserves_row_files_and_competing_lease() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "review/tampered-dismissal-source.pdf";
        let body = b"dismissed review source provenance bytes";
        let (sha, source) = h.seed(rel, std::str::from_utf8(body).unwrap());
        h.pipeline
            .flag(&sha, &source, "SLM_FAIL:review".into(), &clock())
            .await;
        let flagged = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let quarantine_root = PathBuf::from(flagged.quarantine_root.as_ref().unwrap());
        let quarantined = PathBuf::from(flagged.quarantine_path.as_ref().unwrap());
        let mut dismissal =
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &flagged.delivery_id)
                .unwrap()
                .unwrap()
                .manifest;
        dismissal.status = "dismissed".into();
        dismissal.flag_reason = Some("DISMISSED:reviewed".into());
        local_output::record_review(
            &h.pipeline.cfg.local_output_dir,
            &quarantine_root,
            &quarantined,
            &dismissal,
        )
        .unwrap();
        let owner = h
            .pipeline
            .ledger
            .begin_review_operation(&sha, "correct")
            .unwrap()
            .unwrap();
        let before = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let receipt_path = h
            .pipeline
            .cfg
            .local_output_dir
            .join(".backlog/receipts")
            .join(format!("{}.json", before.delivery_id));
        let mut receipt =
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &before.delivery_id)
                .unwrap()
                .unwrap();

        let other_root = h.dir.path().join("other-quarantine");
        std::fs::create_dir_all(&other_root).unwrap();
        receipt.source_root = other_root.to_string_lossy().into_owned();
        std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        let root_tamper = std::fs::read(&receipt_path).unwrap();
        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0
        );
        assert_eq!(std::fs::read(&receipt_path).unwrap(), root_tamper);
        assert_eq!(std::fs::read(&quarantined).unwrap(), body);
        let after_root_tamper = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&after_root_tamper).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(
            after_root_tamper.review_owner.as_deref(),
            Some(owner.as_str())
        );

        let alternate = quarantine_root.join("alternate-same-content.pdf");
        std::fs::write(&alternate, body).unwrap();
        receipt.source_root = quarantine_root.to_string_lossy().into_owned();
        receipt.source_path = alternate.to_string_lossy().into_owned();
        std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        let path_tamper = std::fs::read(&receipt_path).unwrap();
        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0
        );
        assert_eq!(std::fs::read(&receipt_path).unwrap(), path_tamper);
        assert_eq!(std::fs::read(&quarantined).unwrap(), body);
        assert_eq!(std::fs::read(&alternate).unwrap(), body);
        assert!(!source.exists());
        let after_path_tamper = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&after_path_tamper).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(
            after_path_tamper.review_owner.as_deref(),
            Some(owner.as_str())
        );
    }

    #[tokio::test]
    async fn correction_delete_failure_preserves_durable_collision_for_startup_recovery() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "review/delete-failure-collision.pdf";
        let body = b"corrected delivery survives delete failure";
        let (sha, processing_source) = h.seed(rel, std::str::from_utf8(body).unwrap());
        h.pipeline
            .flag(&sha, &processing_source, "SLM_FAIL:review".into(), &clock())
            .await;
        let flagged = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert!(
            flagged.final_filename.is_none(),
            "the regression must begin without a masked reservation"
        );
        let quarantined = PathBuf::from(flagged.quarantine_path.as_ref().unwrap());
        let base_name = "2024-03-05 Acme Corporation Invoice March.pdf";
        std::fs::write(
            h.pipeline.cfg.local_output_dir.join(base_name),
            b"unrelated operator file",
        )
        .unwrap();

        let (date, subject, description) = correction();
        let error = h
            .pipeline
            .resubmit_with_owner_and_remove(&sha, date, subject, description, None, |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected correction source-delete failure",
                ))
            })
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("injected correction source-delete failure"));

        let pending = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let corrected_name = "2024-03-05 Acme Corporation Invoice March (2).pdf";
        assert_eq!(pending.state, JobState::Flagged);
        assert_eq!(pending.final_filename.as_deref(), Some(corrected_name));
        assert_eq!(
            pending.recovery_previous_filename.as_deref(),
            Some(base_name)
        );
        assert_eq!(pending.review_operation.as_deref(), Some("correct"));
        assert!(pending.review_owner.is_some());
        assert!(quarantined.exists());
        let receipt =
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &pending.delivery_id)
                .unwrap()
                .unwrap();
        assert_eq!(receipt.manifest.status, "ok");
        assert_eq!(
            receipt.manifest.new_filename.as_deref(),
            Some(corrected_name)
        );
        assert_eq!(
            std::fs::read(h.pipeline.cfg.local_output_dir.join(base_name)).unwrap(),
            b"unrelated operator file"
        );
        assert_eq!(
            std::fs::read(h.pipeline.cfg.local_output_dir.join(corrected_name)).unwrap(),
            body
        );

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            1
        );
        let recovered = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(recovered.state, JobState::Emitted);
        assert_eq!(recovered.final_filename.as_deref(), Some(corrected_name));
        assert!(recovered.recovery_previous_filename.is_none());
        assert!(recovered.review_operation.is_none());
        assert!(recovered.review_owner.is_none());
        assert!(recovered.quarantine_path.is_none());
        assert!(!quarantined.exists());
        assert!(!processing_source.exists());
        assert_eq!(
            std::fs::read(h.pipeline.cfg.local_output_dir.join(base_name)).unwrap(),
            b"unrelated operator file"
        );
        assert_eq!(
            std::fs::read(h.pipeline.cfg.local_output_dir.join(corrected_name)).unwrap(),
            body
        );
        assert_eq!(
            std::fs::read_dir(h.pipeline.cfg.local_output_dir.join(".backlog/receipts"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_dir(&h.pipeline.cfg.local_output_dir)
                .unwrap()
                .flatten()
                .filter(|entry| entry.path().is_file())
                .count(),
            2,
            "one unrelated file plus exactly one corrected output"
        );
    }

    #[tokio::test]
    async fn local_correction_without_durable_artifact_rolls_back_name_and_lease() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "review/no-durable-artifact.pdf";
        let (sha, source) = h.seed(rel, "original review bytes");
        h.pipeline
            .flag(&sha, &source, "SLM_FAIL:review".into(), &clock())
            .await;
        let flagged = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let quarantined = PathBuf::from(flagged.quarantine_path.as_ref().unwrap());
        std::fs::write(&quarantined, b"changed before correction").unwrap();

        let (date, subject, description) = correction();
        assert!(h
            .pipeline
            .resubmit(&sha, date, subject, description)
            .await
            .is_err());

        let after = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(after.state, JobState::Flagged);
        assert!(after.final_filename.is_none());
        assert!(after.recovery_previous_filename.is_none());
        assert!(after.review_operation.is_none());
        assert!(after.review_owner.is_none());
        assert_eq!(after.quarantine_path.as_deref(), quarantined.to_str());
        assert!(quarantined.exists());
        let receipt =
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &after.delivery_id)
                .unwrap()
                .unwrap();
        assert_eq!(receipt.manifest.status, "flagged");
        assert!(!h
            .pipeline
            .cfg
            .local_output_dir
            .join(".backlog/intents")
            .join(format!("{}.json", after.delivery_id))
            .is_file());
        assert_eq!(
            std::fs::read_dir(&h.pipeline.cfg.local_output_dir)
                .unwrap()
                .flatten()
                .filter(|entry| entry.path().is_file())
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn interrupted_local_correction_recovery_deletes_only_pinned_quarantine_source() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "review/scan.pdf";
        let (sha, processing_source) = h.seed(rel, "corrected quarantine bytes");
        h.pipeline
            .flag(&sha, &processing_source, "SLM_FAIL:review".into(), &clock())
            .await;
        h.pipeline
            .ledger
            .update_fields(
                &sha,
                &[(
                    "final_filename",
                    Some("2024-03-05 Corrected Scan.pdf".into()),
                )],
            )
            .unwrap();
        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let quarantine_source = PathBuf::from(job.quarantine_path.as_ref().unwrap());
        let quarantine_root = PathBuf::from(job.quarantine_root.as_ref().unwrap());
        let _owner = h
            .pipeline
            .ledger
            .begin_review_operation(&sha, "correct")
            .unwrap()
            .unwrap();
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: job.delivery_id.clone(),
            sha256: job.content_sha256.clone(),
            status: "ok".into(),
            original_name: job.original_name.clone(),
            original_relpath: rel.into(),
            new_filename: Some("2024-03-05 Corrected Scan.pdf".into()),
            description: Some("Corrected reviewed scan document.".into()),
            date: Some("2024-03-05".into()),
            date_source: Some("human".into()),
            doc_type: Some("document".into()),
            language: Some("en".into()),
            duplicate_of: None,
            soft_flags: vec!["HUMAN_CORRECTED".into()],
            flag_reason: None,
            model_versions: json!({"convertd": "test"}),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        let error = local_output::deliver_with_remove_for_test(
            &h.pipeline.cfg.local_output_dir,
            &quarantine_root,
            &quarantine_source,
            &manifest,
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected quarantine delete failure",
                ))
            },
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("injected quarantine delete failure"));
        assert!(quarantine_source.exists());
        assert!(!processing_source.exists());
        assert_eq!(
            h.pipeline.ledger.get(&sha).unwrap().unwrap().state,
            JobState::Flagged
        );
        assert!(
            local_output::receipt_is_complete(&h.pipeline.cfg.local_output_dir, &manifest).unwrap()
        );

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            1
        );
        let recovered = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(recovered.state, JobState::Emitted);
        assert!(recovered.review_operation.is_none());
        assert!(recovered.quarantine_path.is_none());
        assert!(recovered.quarantine_planned_path.is_none());
        assert!(recovered.quarantine_root.is_none());
        assert!(!quarantine_source.exists());
        assert!(!processing_source.exists());
        assert_eq!(
            std::fs::read_dir(h.pipeline.cfg.local_output_dir.join(".backlog/receipts"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_dir(&h.pipeline.cfg.local_output_dir)
                .unwrap()
                .flatten()
                .filter(|entry| entry.path().is_file())
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn local_conflicting_review_lease_leaves_intent_row_and_files_unchanged() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "review/lease-conflict.pdf";
        let (sha, source) = h.seed(rel, "local review lease conflict bytes");
        h.pipeline
            .flag(&sha, &source, "SLM_FAIL:review".into(), &clock())
            .await;
        h.pipeline
            .ledger
            .update_fields(
                &sha,
                &[
                    (
                        "final_filename",
                        Some("2026-08-04 Lease Conflict.pdf".into()),
                    ),
                    ("proposed_date", Some("2026-08-04".into())),
                    ("date_source", Some("human".into())),
                    (
                        "description",
                        Some("A correction interrupted by a competing dismissal.".into()),
                    ),
                    ("doc_type", Some("document".into())),
                    ("language", Some("en".into())),
                    ("soft_flags", Some("HUMAN_CORRECTED".into())),
                    (
                        "model_versions",
                        Some(json!({"convertd": "test"}).to_string()),
                    ),
                ],
            )
            .unwrap();
        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let quarantined = PathBuf::from(job.quarantine_path.as_ref().unwrap());
        let quarantine_root = PathBuf::from(job.quarantine_root.as_ref().unwrap());
        let owner = h
            .pipeline
            .ledger
            .begin_review_operation(&sha, "dismiss")
            .unwrap()
            .unwrap();
        let before = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: before.delivery_id.clone(),
            sha256: before.content_sha256.clone(),
            status: "ok".into(),
            original_name: before.original_name.clone(),
            original_relpath: rel.into(),
            new_filename: Some("2026-08-04 Lease Conflict.pdf".into()),
            description: Some("A correction interrupted by a competing dismissal.".into()),
            date: Some("2026-08-04".into()),
            date_source: Some("human".into()),
            doc_type: Some("document".into()),
            language: Some("en".into()),
            duplicate_of: None,
            soft_flags: vec!["HUMAN_CORRECTED".into()],
            flag_reason: None,
            model_versions: json!({"convertd": "test"}),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        let receipt = local_output::Receipt {
            receipt_schema: 1,
            delivery_mode: "local".into(),
            output_relpath: manifest.new_filename.clone(),
            source_root: quarantine_root.to_string_lossy().into_owned(),
            source_path: quarantined.to_string_lossy().into_owned(),
            manifest,
        };
        let mut intent = serde_json::to_value(receipt).unwrap();
        let object = intent.as_object_mut().unwrap();
        object.insert("intent_schema".into(), json!(1));
        object.insert(
            "output_base_name".into(),
            json!("2026-08-04 Lease Conflict.pdf"),
        );
        let intent = serde_json::to_vec(&intent).unwrap();
        let intent_path = h
            .pipeline
            .cfg
            .local_output_dir
            .join(".backlog/intents")
            .join(format!("{}.json", before.delivery_id));
        std::fs::create_dir_all(intent_path.parent().unwrap()).unwrap();
        std::fs::write(&intent_path, &intent).unwrap();
        let flagged_receipt = std::fs::read(
            h.pipeline
                .cfg
                .local_output_dir
                .join(".backlog/receipts")
                .join(format!("{}.json", before.delivery_id)),
        )
        .unwrap();

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0
        );
        let after = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(after.review_owner.as_deref(), Some(owner.as_str()));
        assert!(quarantined.exists());
        assert!(!source.exists());
        assert!(!h
            .pipeline
            .cfg
            .local_output_dir
            .join("2026-08-04 Lease Conflict.pdf")
            .exists());
        assert_eq!(std::fs::read(&intent_path).unwrap(), intent);
        assert_eq!(
            std::fs::read(
                h.pipeline
                    .cfg
                    .local_output_dir
                    .join(".backlog/receipts")
                    .join(format!("{}.json", before.delivery_id)),
            )
            .unwrap(),
            flagged_receipt
        );
    }

    #[tokio::test]
    async fn pa_conflicting_review_lease_leaves_manifest_and_row_unchanged() {
        let h = Harness::new();
        let rel = "review/pa-lease-conflict.pdf";
        let (sha, source) = h.seed(rel, "PA review lease conflict bytes");
        h.pipeline
            .flag(&sha, &source, "SLM_FAIL:review".into(), &clock())
            .await;
        let owner = h
            .pipeline
            .ledger
            .begin_review_operation(&sha, "dismiss")
            .unwrap()
            .unwrap();
        let before = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let quarantined = PathBuf::from(before.quarantine_path.as_ref().unwrap());
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: before.delivery_id.clone(),
            sha256: before.content_sha256.clone(),
            status: "ok".into(),
            original_name: before.original_name.clone(),
            original_relpath: rel.into(),
            new_filename: Some("2026-08-04 PA Lease Conflict.pdf".into()),
            description: Some("A PA correction interrupted by a competing dismissal.".into()),
            date: Some("2026-08-04".into()),
            date_source: Some("human".into()),
            doc_type: Some("document".into()),
            language: Some("en".into()),
            duplicate_of: None,
            soft_flags: vec!["HUMAN_CORRECTED".into()],
            flag_reason: None,
            model_versions: json!({"convertd": "test"}),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        write_manifest(&h.pipeline.cfg.manifests_dir(), &manifest).unwrap();
        let manifest_path = h
            .pipeline
            .cfg
            .manifests_dir()
            .join(format!("{}.json", before.delivery_id));
        let manifest_bytes = std::fs::read(&manifest_path).unwrap();

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0
        );
        let after = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(after.review_owner.as_deref(), Some(owner.as_str()));
        assert!(quarantined.exists());
        assert!(!source.exists());
        assert_eq!(std::fs::read(manifest_path).unwrap(), manifest_bytes);
    }

    #[test]
    fn pa_duplicate_manifest_recovers_by_pinned_delivery_id_after_source_is_gone() {
        let h = Harness::new();
        let rel = "copies/invoice.pdf";
        let source = h.pipeline.cfg.processing_dir.join(rel);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"duplicate crash recovery bytes").unwrap();
        let content_sha = hash_file(&source).unwrap();
        let duplicate_id = manifest_id(&content_sha, rel);
        h.pipeline
            .ledger
            .ingest_with_delivery(
                &duplicate_id,
                &source.to_string_lossy(),
                "invoice.pdf",
                rel,
                "pdf",
                "power_automate",
                &h.pipeline.cfg.outbox_dir.to_string_lossy(),
                &h.pipeline.cfg.processing_dir.to_string_lossy(),
                &content_sha,
            )
            .unwrap();
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: duplicate_id.clone(),
            sha256: content_sha.clone(),
            status: "ok".into(),
            original_name: "invoice.pdf".into(),
            original_relpath: rel.into(),
            new_filename: Some("2026-08-04 Invoice (2).pdf".into()),
            description: Some("A duplicate committed before the ledger CAS.".into()),
            date: Some("2026-08-04".into()),
            date_source: Some("document".into()),
            doc_type: Some("invoice".into()),
            language: Some("en".into()),
            duplicate_of: Some(content_sha.clone()),
            soft_flags: vec!["DUPLICATE_CONTENT".into()],
            flag_reason: None,
            model_versions: json!({ "convertd": "test" }),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        write_manifest(&h.pipeline.cfg.manifests_dir(), &manifest).unwrap();
        std::fs::remove_file(&source).unwrap(); // Flow 2 consumed it before restart.

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            1
        );
        let job = h.pipeline.ledger.get(&duplicate_id).unwrap().unwrap();
        assert_eq!(job.state, JobState::Emitted);
        assert_eq!(job.delivery_id, duplicate_id);
        assert_eq!(job.content_sha256, content_sha);
    }

    #[tokio::test]
    async fn unresolved_same_content_copy_is_deferred_then_gets_its_own_duplicate_delivery() {
        let h = Harness::new();
        let (sha, original) = h.seed("a/original.pdf", "same content in two paths");
        let copy = h.pipeline.cfg.processing_dir.join("b/copy.pdf");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::copy(&original, &copy).unwrap();
        let copy_rel = "b/copy.pdf";
        let duplicate_id = manifest_id(&sha, copy_rel);

        let deferred = tokio::spawn(h.pipeline.clone().process_file(copy.clone()));
        // Test retry timing maps one configured second to 10 ms. Resolve only
        // after the former fixed 30 x 10 ms (300 ms) window has elapsed: the
        // single enqueue must still create this copy's own delivery.
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        assert!(
            copy.exists(),
            "an unresolved original must not consume its copy"
        );
        assert!(h.pipeline.ledger.get(&duplicate_id).unwrap().is_none());
        assert_eq!(
            h.pipeline.ledger.get(&sha).unwrap().unwrap().state,
            JobState::Ingested
        );

        h.pipeline
            .ledger
            .update_fields(
                &sha,
                &[
                    ("final_filename", Some("original.pdf".into())),
                    (
                        "description",
                        Some("Original duplicate fixture document.".into()),
                    ),
                    ("proposed_date", Some("2026-08-04".into())),
                    ("date_source", Some("document".into())),
                    ("doc_type", Some("document".into())),
                    ("language", Some("en".into())),
                ],
            )
            .unwrap();
        h.pipeline
            .ledger
            .set_state(&sha, JobState::Emitted)
            .unwrap();
        deferred.await.unwrap();
        let duplicate = h.pipeline.ledger.get(&duplicate_id).unwrap().unwrap();
        assert_eq!(duplicate.state, JobState::Emitted);
        assert_eq!(duplicate.delivery_id, duplicate_id);
        assert_eq!(duplicate.content_sha256, sha);
        assert!(
            copy.exists(),
            "PA duplicate delivery never deletes the physical copy"
        );
    }

    /// A batch can keep the original queued for longer than the configured
    /// terminal window. The parked copy must keep waiting for the original's
    /// terminal transition — the original always reaches one, because its own
    /// wall clock flags it — instead of abandoning the copy invisibly in
    /// Processing (the v0.8.0 silent-drop bug).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_content_copy_outlives_the_terminal_window_and_still_delivers() {
        let h = Harness::new();
        let (sha, original) = h.seed("a/original.pdf", "same content, slow original");
        let copy = h.pipeline.cfg.processing_dir.join("b/copy.pdf");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::copy(&original, &copy).unwrap();
        let duplicate_id = manifest_id(&sha, "b/copy.pdf");

        let deferred = tokio::spawn(h.pipeline.clone().process_file(copy.clone()));
        // Test timing maps one configured second to 10 ms, so the terminal
        // window here is per_file_wall_clock_secs * 10 ms (900 ms on the
        // default config). Resolve the original only after that window has
        // fully elapsed.
        let window = deferred_duplicate_retry_window(&h.pipeline.cfg);
        tokio::time::sleep(window + std::time::Duration::from_millis(250)).await;
        assert!(
            h.pipeline.ledger.get(&duplicate_id).unwrap().is_none(),
            "the copy must not resolve before its original"
        );

        h.pipeline
            .ledger
            .update_fields(
                &sha,
                &[
                    ("final_filename", Some("original.pdf".into())),
                    (
                        "description",
                        Some("Original duplicate fixture document.".into()),
                    ),
                    ("proposed_date", Some("2026-08-04".into())),
                    ("date_source", Some("document".into())),
                    ("doc_type", Some("document".into())),
                    ("language", Some("en".into())),
                ],
            )
            .unwrap();
        h.pipeline
            .ledger
            .set_state(&sha, JobState::Emitted)
            .unwrap();
        deferred.await.unwrap();
        let duplicate = h.pipeline.ledger.get(&duplicate_id).unwrap().unwrap();
        assert_eq!(
            duplicate.state,
            JobState::Emitted,
            "a copy that outlived the window must still get its own delivery"
        );
        assert!(copy.exists());
    }

    /// A duplicate of a flagged original has nothing sane to emit, but the
    /// physical copy must not rot invisibly in Processing (the v0.8.0
    /// zero-byte silent-drop bug: every empty file shares one sha, so the
    /// second one vanished forever). Park the copy in quarantine beside its
    /// original so an operator sees exactly one story.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_of_a_flagged_original_is_quarantined_not_abandoned() {
        let h = Harness::new();
        let (sha, _original) = h.seed("a/original.pdf", "content that got flagged");
        h.pipeline
            .ledger
            .set_state(&sha, JobState::Flagged)
            .unwrap();

        let copy = h.pipeline.cfg.processing_dir.join("b/copy.pdf");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::write(&copy, "content that got flagged").unwrap();
        h.pipeline.clone().process_file(copy.clone()).await;

        assert!(
            !copy.exists(),
            "the copy must leave Processing instead of rotting there invisibly"
        );
        let entries = h.quarantine_entries();
        assert!(
            entries.iter().any(|n| n.ends_with("__copy.pdf")),
            "the copy must be visible in quarantine, got: {entries:?}"
        );
    }

    /// What the model actually proposes, and what the checker says about it.
    ///
    /// `#[ignore]`, and it prints rather than asserts: the point is to see the
    /// raw proposal beside the rule that refused it. A flagged document's
    /// manifest carries only a code, deliberately — `CheckError::code` exists
    /// precisely so a rejection can be persisted without the document-derived
    /// text — so this is the sanctioned place to look at the text itself, in a
    /// developer's terminal on fixtures, never in a log or an index.
    ///
    /// Same environment variables as `e2e_real_batch`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "diagnostic: needs real sidecars and weights; prints, does not assert"]
    async fn e2e_what_the_model_proposes() {
        fn env_path(key: &str) -> PathBuf {
            PathBuf::from(std::env::var(key).unwrap_or_else(|_| panic!("{key} must be set")))
        }
        let processing = env_path("BACKLOG_E2E_PROCESSING");
        let primary = env_path("BACKLOG_E2E_PRIMARY");
        let escalation = std::env::var("BACKLOG_E2E_ESCALATION")
            .map(PathBuf::from)
            .unwrap_or_else(|_| primary.clone());

        let cfg = Config {
            slm_primary_gguf: primary,
            slm_escalation_gguf: escalation,
            slm_parallel: 1,
            ..Default::default()
        };
        let sidecar = Sidecar::with_timeout(
            env_path("BACKLOG_E2E_CONVERTD"),
            std::time::Duration::from_secs(cfg.sidecar_timeout_secs),
        )
        .with_models_dir(cfg.slm_primary_gguf.parent().unwrap().to_path_buf());
        let slm = SlmLane::new(
            env_path("BACKLOG_E2E_LLAMA"),
            String::new(),
            cfg.slm_primary_gguf.clone(),
            cfg.slm_escalation_gguf.clone(),
            cfg.llama_port,
            cfg.slm_parallel,
            cfg.slm_threads(),
        );
        let checker = crate::checker::Checker::new(cfg.max_filename_len);

        let mut files: Vec<PathBuf> = std::fs::read_dir(&processing)
            .expect("Processing folder must exist")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();

        for path in files {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let conv = match sidecar.convert(&path.to_string_lossy(), 10, 3) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("\n### {name}\n  convert failed: {e}");
                    continue;
                }
            };
            let outcome = match filter::build_evidence(
                &sidecar,
                &conv.markdown,
                conv.doc_meta_dates.clone(),
                false,
                cfg.evidence_token_budget,
            ) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("\n### {name}\n  evidence failed: {e}");
                    continue;
                }
            };
            let ev = outcome.evidence;
            // Mirrors `process_inner`: embedded metadata is evidence, the file's
            // own timestamps are not.
            let (_fs_dates, modified_iso) = crate::checker::fs_metadata_dates(&path);
            let mut meta_dates = outcome.doc_meta_dates;
            meta_dates.dedup();

            eprintln!("\n### {name}");
            eprintln!(
                "  harvested dates: {} | thin: {}",
                ev.harvest.dates.len(),
                ev.thin
            );
            match slm
                .name_document(
                    crate::slm::Tier::Primary,
                    &ev.bundle,
                    ev.doc_type.as_deref().unwrap_or("unknown"),
                    &ev.language,
                    None,
                )
                .await
            {
                Ok(out) => {
                    let words = out
                        .subject
                        .split([' ', '-', '_', '/'])
                        .filter(|w| !w.is_empty())
                        .count();
                    eprintln!(
                        "  subject ({words} words, {} chars): {:?}",
                        out.subject.chars().count(),
                        out.subject
                    );
                    eprintln!(
                        "  description ({} chars): {:?}",
                        out.description.chars().count(),
                        out.description
                    );
                    eprintln!("  date: {:?} source: {:?}", out.date, out.date_source);
                    match checker.check(&out, &ev.harvest, &meta_dates, &modified_iso, None) {
                        Ok(v) => {
                            eprintln!("  ACCEPTED -> {:?} flags={:?}", v.base_name, v.soft_flags)
                        }
                        Err(ce) => eprintln!("  REJECTED [{}] {}", ce.code(), ce),
                    }
                }
                Err(e) => eprintln!("  SLM error: {e}"),
            }
        }
    }

    /// A real batch, through the real binaries, against real folders.
    ///
    /// `#[ignore]` because it needs things no gate can assume: the built
    /// sidecars, ~2.5 GB of GGUF weights, and minutes of CPU. It is not part of
    /// the five gates and never runs in `cargo test`. It exists because
    /// everything above this line drives the orchestrator against stubs, so
    /// nothing in the suite could answer "does a thousand tax PDFs actually
    /// come out the other end, and what does it cost in RAM and wall clock" —
    /// which is the only question a pilot deployment cares about.
    ///
    /// Every path comes from the environment so this is not wired to one
    /// machine:
    ///
    /// ```powershell
    /// $env:BACKLOG_E2E_PROCESSING = "C:\...\Processing"
    /// $env:BACKLOG_E2E_OUTBOX     = "C:\...\Outbox"
    /// $env:BACKLOG_E2E_QUARANTINE = "C:\...\Quarantine"
    /// $env:BACKLOG_E2E_CONVERTD   = "C:\...\convertd.exe"
    /// $env:BACKLOG_E2E_LLAMA      = "C:\...\llama-server.exe"
    /// $env:BACKLOG_E2E_PRIMARY    = "C:\...\Qwen3-0.6B-Q8_0.gguf"
    /// $env:BACKLOG_E2E_ESCALATION = "C:\...\Qwen3-1.7B-Q8_0.gguf"   # optional
    /// $env:BACKLOG_E2E_PARALLEL   = "2"                              # optional
    /// $env:BACKLOG_E2E_WORKERS    = "3"                              # optional
    /// cargo test -p backlog --lib --release e2e_real_batch -- --ignored --nocapture
    /// ```
    ///
    /// It prints a per-file outcome table and a summary; it asserts only that
    /// every file reached a terminal state and that nothing vanished, because
    /// the naming quality of a local model on synthetic paperwork is a judgment
    /// call for a human reading the table, not something to encode as a
    /// threshold that would make this fail for the wrong reason.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs real sidecars, real GGUF weights and minutes of CPU; run explicitly"]
    async fn e2e_real_batch() {
        fn env_path(key: &str) -> PathBuf {
            PathBuf::from(
                std::env::var(key).unwrap_or_else(|_| panic!("{key} must be set for this test")),
            )
        }
        fn env_num(key: &str, default: usize) -> usize {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }

        let processing = env_path("BACKLOG_E2E_PROCESSING");
        let outbox = env_path("BACKLOG_E2E_OUTBOX");
        let quarantine = env_path("BACKLOG_E2E_QUARANTINE");
        let convertd_exe = env_path("BACKLOG_E2E_CONVERTD");
        let llama_exe = env_path("BACKLOG_E2E_LLAMA");
        let primary = env_path("BACKLOG_E2E_PRIMARY");
        // Falling back to the primary is the supported 8 GB shape: the
        // escalation rung still runs, just against the model already resident,
        // instead of standing a second llama-server up beside it.
        let escalation = std::env::var("BACKLOG_E2E_ESCALATION")
            .map(PathBuf::from)
            .unwrap_or_else(|_| primary.clone());
        let parallel = env_num("BACKLOG_E2E_PARALLEL", 2);
        let workers = env_num("BACKLOG_E2E_WORKERS", 3);

        let work = std::env::temp_dir().join("backlog-e2e");
        let cache_dir = work.join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        for d in [&outbox, &quarantine] {
            std::fs::create_dir_all(d).unwrap();
        }

        let cfg = Config {
            processing_dir: processing.clone(),
            outbox_dir: outbox.clone(),
            quarantine_dir: quarantine.clone(),
            cache_dir,
            slm_primary_gguf: primary,
            slm_escalation_gguf: escalation,
            slm_parallel: parallel as u8,
            convert_workers: workers,
            manifest_emit_per_min: 0,
            ..Default::default()
        };
        cfg.validate()
            .expect("the three folders must be distinct and non-nested");

        let ledger = Arc::new(Ledger::open(&work.join("ledger.db")).unwrap());
        let sidecar = Arc::new(
            Sidecar::with_timeout(
                convertd_exe,
                std::time::Duration::from_secs(cfg.sidecar_timeout_secs),
            )
            .with_models_dir(cfg.slm_primary_gguf.parent().unwrap().to_path_buf()),
        );
        let grammar =
            std::fs::read_to_string(std::env::var("BACKLOG_E2E_GRAMMAR").unwrap_or_default())
                .unwrap_or_default();
        let slm = Arc::new(SlmLane::new(
            llama_exe,
            grammar,
            cfg.slm_primary_gguf.clone(),
            cfg.slm_escalation_gguf.clone(),
            cfg.llama_port,
            cfg.slm_parallel,
            cfg.slm_threads(),
        ));
        let model_versions = sidecar.versions().unwrap_or_else(|_| json!({}));
        assert!(
            model_versions.get("convertd").is_some(),
            "convertd did not answer `versions`; a manifest cannot carry provenance without it: {model_versions}"
        );

        let pipeline = Arc::new(Pipeline {
            convert_slots: Arc::new(Semaphore::new(cfg.convert_workers.max(1))),
            slm_slots: Arc::new(Semaphore::new(cfg.slm_parallel.max(1) as usize)),
            ingest_slots: Arc::new(Semaphore::new(
                (cfg.convert_workers.max(1) * 4).clamp(8, 64),
            )),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            pacer: Arc::new(Pacer::new(cfg.manifest_emit_per_min)),
            cfg,
            ledger: ledger.clone(),
            sidecar,
            slm,
            app: None,
            paused: Arc::new(AtomicBool::new(false)),
            model_versions,
        });

        let mut files: Vec<PathBuf> = std::fs::read_dir(&processing)
            .expect("Processing folder must exist")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no files in {}", processing.display());
        let total = files.len();
        eprintln!(
            "\n=== e2e: {total} files, slm_parallel={parallel}, convert_workers={workers} ==="
        );

        let started = std::time::Instant::now();
        let mut handles = Vec::new();
        for path in files {
            let p = pipeline.clone();
            handles.push(tokio::spawn(async move {
                let t = std::time::Instant::now();
                p.process_file(path.clone()).await;
                (path, t.elapsed())
            }));
        }
        let mut per_file = Vec::new();
        for h in handles {
            per_file.push(h.await.expect("no worker may panic"));
        }
        let wall = started.elapsed();

        // Outcome comes from the ledger, not from guessing at the filesystem.
        let manifests = std::fs::read_dir(outbox.join("_manifests"))
            .map(|d| d.flatten().filter(|e| e.path().is_file()).count())
            .unwrap_or(0);
        let quarantined = std::fs::read_dir(&quarantine)
            .map(|d| d.flatten().filter(|e| e.path().is_file()).count())
            .unwrap_or(0);
        let left = std::fs::read_dir(&processing)
            .map(|d| d.flatten().filter(|e| e.path().is_file()).count())
            .unwrap_or(0);

        // Statuses come from the manifests, which are the contract Flow 2 reads.
        let mut ok = 0usize;
        let mut flagged = 0usize;
        let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
        if let Ok(entries) = std::fs::read_dir(outbox.join("_manifests")) {
            for e in entries.flatten() {
                let Ok(bytes) = std::fs::read(e.path()) else {
                    continue;
                };
                let Ok(m) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                    continue;
                };
                match m.get("status").and_then(|s| s.as_str()) {
                    Some("ok") => ok += 1,
                    Some(_) => {
                        flagged += 1;
                        let reason = m
                            .get("flag_reason")
                            .and_then(|r| r.as_str())
                            .unwrap_or("(none)")
                            .to_string();
                        *reasons.entry(reason).or_default() += 1;
                    }
                    None => {}
                }
            }
        }

        let mut slowest: Vec<_> = per_file.iter().collect();
        slowest.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
        eprintln!("\n--- ten slowest ---");
        for (path, d) in slowest.iter().take(10) {
            eprintln!(
                "  {:>7.1}s  {}",
                d.as_secs_f64(),
                path.file_name().unwrap().to_string_lossy()
            );
        }
        let secs = wall.as_secs_f64();
        eprintln!("\n--- flag reasons ---");
        for (reason, n) in &reasons {
            eprintln!("  {n:>4}  {reason}");
        }
        eprintln!(
            "\n=== {total} files in {:.1}s ({:.2} s/file) ===",
            secs,
            secs / total as f64
        );
        eprintln!(
            "=== named ok {ok}/{total} ({:.0}%) | flagged {flagged} | manifests {manifests} | quarantined {quarantined} | left in Processing {left} ===",
            100.0 * ok as f64 / total as f64
        );
        eprintln!(
            "=== extrapolated to 1,000 files: {:.1} hours ===\n",
            (secs / total as f64) * 1000.0 / 3600.0
        );

        // The invariant is one manifest per file, not one *outcome* per file: a
        // flagged document produces a manifest **and** a quarantine copy, so
        // `manifests + quarantined` double-counts every failure. Asserting that
        // sum is how this test first "failed" against a pipeline that was
        // behaving exactly as designed.
        assert_eq!(
            manifests, total,
            "every file must produce exactly one manifest: {manifests} for {total} files"
        );
        assert_eq!(
            ok + flagged,
            total,
            "every manifest must be ok or flagged: {ok} ok + {flagged} flagged != {total}"
        );
        assert_eq!(
            quarantined, flagged,
            "every flagged file must be in quarantine and nothing else should be: {quarantined} quarantined vs {flagged} flagged"
        );
        assert_eq!(
            left, ok,
            "an ok file stays in Processing for Flow 2 to move; a flagged one must not: {left} left vs {ok} ok"
        );
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
                2,
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

    /// A terminal state is earned only after both the review file and its
    /// manifest are durable. A transient outbox failure must leave the source
    /// in Processing so the watcher can retry it.
    #[tokio::test]
    async fn failed_flag_manifest_restores_the_source_and_stays_retryable() {
        let h = Harness::new();
        let (sha, path) = h.seed("retry/scan.pdf", "retryable document");
        let manifests = h.pipeline.cfg.manifests_dir();
        std::fs::write(&manifests, b"blocks create_dir_all").unwrap();

        h.pipeline.clone().process_file(path.clone()).await;

        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(job.state, JobState::Ingested);
        assert!(path.exists(), "the next watcher event must find the source");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "retryable document"
        );
        assert!(
            h.quarantine_entries().is_empty(),
            "rollback must not leave a second physical copy"
        );
        assert!(h.pipeline.ledger.try_claim(&sha, 3_600).unwrap());
    }

    #[tokio::test]
    async fn valid_existing_ok_manifest_recovers_ledger_without_model_work() {
        let h = Harness::new();
        let rel = "recover/invoice.pdf";
        let (sha, path) = h.seed(rel, "recoverable document");
        h.pipeline
            .ledger
            .set_state(&sha, JobState::Validated)
            .unwrap();
        let expected_name = "2024-03-05 Acme Invoice.pdf";
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: manifest_id(&sha, rel),
            sha256: sha.clone(),
            status: "ok".into(),
            original_name: "invoice.pdf".into(),
            original_relpath: rel.into(),
            new_filename: Some(expected_name.into()),
            description: Some("Invoice from Acme Corporation for consulting services.".into()),
            date: Some("2024-03-05".into()),
            date_source: Some("document".into()),
            doc_type: Some("invoice".into()),
            language: Some("en".into()),
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: None,
            model_versions: json!({"convertd": "test"}),
            processed_at: "2026-07-30T00:00:00Z".into(),
        };
        write_manifest(&h.pipeline.cfg.manifests_dir(), &manifest).unwrap();

        h.pipeline.clone().process_file(path).await;
        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(job.state, JobState::Emitted);
        assert_eq!(job.final_filename.as_deref(), Some(expected_name));
        assert_eq!(job.proposed_date.as_deref(), Some("2024-03-05"));
    }

    #[test]
    fn startup_reconciles_flagged_manifest_without_a_processing_source() {
        let h = Harness::new();
        let rel = "recover/scan.pdf";
        let (sha, path) = h.seed(rel, "flagged before the crash");
        h.pipeline
            .ledger
            .set_state(&sha, JobState::Converted)
            .unwrap();

        let quarantined = h.pipeline.cfg.quarantine_dir.join("interrupted-scan.pdf");
        std::fs::rename(&path, &quarantined).unwrap();
        h.pipeline
            .ledger
            .update_fields(
                &sha,
                &[
                    ("flag_reason", Some("UNREADABLE:scan".into())),
                    (
                        "quarantine_path",
                        Some(quarantined.to_string_lossy().into_owned()),
                    ),
                ],
            )
            .unwrap();
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: manifest_id(&sha, rel),
            sha256: sha.clone(),
            status: "flagged".into(),
            original_name: "scan.pdf".into(),
            original_relpath: rel.into(),
            new_filename: None,
            description: None,
            date: None,
            date_source: None,
            doc_type: None,
            language: None,
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: Some("UNREADABLE:scan".into()),
            model_versions: json!({"convertd": "test"}),
            processed_at: "2026-07-30T00:00:00Z".into(),
        };
        write_manifest(&h.pipeline.cfg.manifests_dir(), &manifest).unwrap();

        assert!(!path.exists(), "the watcher cannot rediscover this source");
        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            1
        );
        assert_eq!(
            h.pipeline.ledger.get(&sha).unwrap().unwrap().state,
            JobState::Flagged
        );
        assert!(quarantined.is_file());
    }

    #[test]
    fn failed_source_delete_removes_the_cross_volume_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("processing.pdf");
        let destination = dir.path().join("quarantine.pdf");
        std::fs::write(&source, b"one authoritative copy").unwrap();

        let error = copy_then_remove_with(&source, &destination, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "source is locked",
            ))
        })
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            source.is_file(),
            "the retry source must remain in Processing"
        );
        assert!(
            !destination.exists(),
            "a failed move must not leave a quarantine duplicate"
        );
    }

    #[tokio::test]
    async fn mismatched_existing_manifest_does_not_bypass_model_work() {
        let h = Harness::new();
        let rel = "recover/invoice.pdf";
        let (sha, path) = h.seed(rel, "recoverable document");
        let wrong_sha = "f".repeat(64);
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: manifest_id(&sha, rel),
            sha256: wrong_sha,
            status: "flagged".into(),
            original_name: "invoice.pdf".into(),
            original_relpath: rel.into(),
            new_filename: None,
            description: None,
            date: None,
            date_source: None,
            doc_type: None,
            language: None,
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: Some("UNREADABLE:test".into()),
            model_versions: json!({}),
            processed_at: "2026-07-30T00:00:00Z".into(),
        };
        write_manifest(&h.pipeline.cfg.manifests_dir(), &manifest).unwrap();

        h.pipeline.clone().process_file(path).await;

        let events = h.pipeline.ledger.events_for(&sha, 20).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.stage == "convert" && event.detail.contains("failed")),
            "an identity mismatch must continue through real converter work: {events:?}"
        );
        assert!(!h
            .pipeline
            .ledger
            .get(&sha)
            .unwrap()
            .unwrap()
            .state
            .is_resolved());
    }

    #[tokio::test]
    async fn malformed_existing_manifest_does_not_bypass_model_work() {
        let h = Harness::new();
        let rel = "recover/malformed.pdf";
        let (sha, path) = h.seed(rel, "recoverable document");
        let manifest_path = h
            .pipeline
            .cfg
            .manifests_dir()
            .join(format!("{}.json", manifest_id(&sha, rel)));
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, b"{ this is not json").unwrap();

        h.pipeline.clone().process_file(path).await;

        let events = h.pipeline.ledger.events_for(&sha, 20).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.stage == "convert" && event.detail.contains("failed")),
            "malformed JSON must continue through real converter work: {events:?}"
        );
        assert!(!h
            .pipeline
            .ledger
            .get(&sha)
            .unwrap()
            .unwrap()
            .state
            .is_resolved());
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

    fn durable_pa_correction(h: &Harness, sha: &str, rel: &str) -> Job {
        let job = h.pipeline.ledger.get(sha).unwrap().unwrap();
        let (date, subject, description) = correction();
        let filename = h
            .pipeline
            .ledger
            .reserve_name(&format!("{date} {subject}"), &job.ext, sha)
            .unwrap();
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: job.delivery_id.clone(),
            sha256: job.content_sha256.clone(),
            status: "ok".into(),
            original_name: job.original_name.clone(),
            original_relpath: rel.into(),
            new_filename: Some(filename),
            description: Some(description),
            date: Some(date),
            date_source: Some("human".into()),
            doc_type: job.doc_type.clone(),
            language: job.language.clone(),
            duplicate_of: manifest_duplicate_of(&job),
            soft_flags: vec!["HUMAN_CORRECTED".into()],
            flag_reason: None,
            model_versions: json!({"convertd": "test"}),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        write_manifest(&h.pipeline.cfg.manifests_dir(), &manifest).unwrap();
        h.pipeline
            .ledger
            .begin_review_operation(sha, "correct")
            .unwrap()
            .expect("flagged correction must acquire its owner");
        h.pipeline.ledger.get(sha).unwrap().unwrap()
    }

    #[tokio::test]
    async fn pa_correction_restore_failure_stays_flagged_then_restart_converges() {
        let h = Harness::new();
        let rel = "restore/retry.pdf";
        let (sha, processing) = h.seed(rel, "PA restore retry bytes");
        h.pipeline
            .flag(&sha, &processing, "SLM_FAIL:review".into(), &clock())
            .await;
        let pending = durable_pa_correction(&h, &sha, rel);
        let quarantined = PathBuf::from(pending.quarantine_path.as_ref().unwrap());
        let before = serde_json::to_value(&pending).unwrap();

        assert!(restore_power_automate_correction_with(
            &h.pipeline.cfg,
            &pending,
            rel,
            |_, _| Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected rename failure"
            )),
        )
        .is_err());
        assert_eq!(
            serde_json::to_value(h.pipeline.ledger.get(&sha).unwrap().unwrap()).unwrap(),
            before,
            "rename failure must retain the exact flagged restoration record"
        );
        assert!(quarantined.exists());
        assert!(!processing.exists());

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            1,
            "restart must restore then terminalize the durable correction"
        );
        let complete = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(complete.state, JobState::Emitted);
        assert!(complete.quarantine_path.is_none());
        assert!(complete.quarantine_planned_path.is_none());
        assert!(complete.quarantine_root.is_none());
        assert!(processing.exists());
        assert!(!quarantined.exists());
    }

    #[tokio::test]
    async fn pa_correction_restart_after_move_before_terminal_cas_is_idempotent() {
        let h = Harness::new();
        let rel = "restore/after-move.pdf";
        let (sha, processing) = h.seed(rel, "PA restore crash boundary bytes");
        h.pipeline
            .flag(&sha, &processing, "SLM_FAIL:review".into(), &clock())
            .await;
        let pending = durable_pa_correction(&h, &sha, rel);
        let quarantined = PathBuf::from(pending.quarantine_path.as_ref().unwrap());
        restore_power_automate_correction(&h.pipeline.cfg, &pending, rel).unwrap();
        assert!(processing.exists());
        assert!(!quarantined.exists());
        assert_eq!(
            h.pipeline.ledger.get(&sha).unwrap().unwrap().state,
            JobState::Flagged,
            "models crash after the move and before the terminal CAS"
        );

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            1
        );
        assert_eq!(
            h.pipeline.ledger.get(&sha).unwrap().unwrap().state,
            JobState::Emitted
        );
    }

    #[tokio::test]
    async fn pa_correction_restore_never_replaces_a_foreign_processing_file() {
        let h = Harness::new();
        let rel = "restore/foreign.pdf";
        let (sha, processing) = h.seed(rel, "quarantined authoritative bytes");
        h.pipeline
            .flag(&sha, &processing, "SLM_FAIL:review".into(), &clock())
            .await;
        let pending = durable_pa_correction(&h, &sha, rel);
        let quarantined = PathBuf::from(pending.quarantine_path.as_ref().unwrap());
        std::fs::create_dir_all(processing.parent().unwrap()).unwrap();
        std::fs::write(&processing, b"foreign replacement").unwrap();

        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0
        );
        let after = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(after.state, JobState::Flagged);
        assert_eq!(
            after.quarantine_path.as_deref(),
            pending.quarantine_path.as_deref()
        );
        assert_eq!(std::fs::read(&processing).unwrap(), b"foreign replacement");
        assert_eq!(
            std::fs::read(&quarantined).unwrap(),
            b"quarantined authoritative bytes"
        );
    }

    #[tokio::test]
    async fn local_flagged_duplicate_correction_preserves_delivery_and_content_identity() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "copies/local-scan.pdf";
        let body = b"local physical duplicate correction bytes";
        let (ledger_key, content_sha, source) =
            h.seed_duplicate(rel, std::str::from_utf8(body).unwrap());
        assert_ne!(ledger_key, content_sha);

        h.pipeline
            .flag(
                &ledger_key,
                &source,
                "SLM_FAIL:duplicate review".into(),
                &clock(),
            )
            .await;
        let flagged_job = h.pipeline.ledger.get(&ledger_key).unwrap().unwrap();
        assert_eq!(flagged_job.state, JobState::Flagged);
        let quarantined = PathBuf::from(flagged_job.quarantine_path.as_ref().unwrap());
        assert_eq!(std::fs::read(&quarantined).unwrap(), body);
        let flagged =
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &flagged_job.delivery_id)
                .unwrap()
                .unwrap();
        assert_eq!(flagged.manifest.manifest_id, ledger_key);
        assert_eq!(flagged.manifest.sha256, content_sha);
        assert_eq!(
            flagged.manifest.duplicate_of.as_deref(),
            Some(content_sha.as_str())
        );

        let (date, subject, description) = correction();
        h.pipeline
            .resubmit(&ledger_key, date, subject, description)
            .await
            .unwrap();

        let receipt = local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &ledger_key)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.manifest.status, "ok");
        assert_eq!(receipt.manifest.manifest_id, ledger_key);
        assert_eq!(receipt.manifest.sha256, content_sha);
        assert_eq!(
            receipt.manifest.duplicate_of.as_deref(),
            Some(content_sha.as_str())
        );
        let output = h
            .pipeline
            .cfg
            .local_output_dir
            .join(receipt.manifest.new_filename.as_deref().unwrap());
        assert_eq!(std::fs::read(output).unwrap(), body);
        assert!(!source.exists());
        assert!(!quarantined.exists());
        let corrected_job = h.pipeline.ledger.get(&ledger_key).unwrap().unwrap();
        assert_eq!(corrected_job.state, JobState::Emitted);
        assert!(corrected_job.quarantine_path.is_none());
    }

    #[tokio::test]
    async fn pa_flagged_duplicate_correction_preserves_delivery_and_content_identity() {
        let h = Harness::new();
        let rel = "copies/pa-scan.pdf";
        let body = b"pa physical duplicate correction bytes";
        let (ledger_key, content_sha, source) =
            h.seed_duplicate(rel, std::str::from_utf8(body).unwrap());
        assert_ne!(ledger_key, content_sha);

        h.pipeline
            .flag(
                &ledger_key,
                &source,
                "SLM_FAIL:duplicate review".into(),
                &clock(),
            )
            .await;
        let flagged_job = h.pipeline.ledger.get(&ledger_key).unwrap().unwrap();
        assert_eq!(flagged_job.state, JobState::Flagged);
        let quarantined = PathBuf::from(flagged_job.quarantine_path.as_ref().unwrap());
        assert_eq!(std::fs::read(&quarantined).unwrap(), body);
        let manifest_path = h
            .pipeline
            .cfg
            .manifests_dir()
            .join(format!("{ledger_key}.json"));
        let flagged: Manifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(flagged.manifest_id, ledger_key);
        assert_eq!(flagged.sha256, content_sha);
        assert_eq!(flagged.duplicate_of.as_deref(), Some(content_sha.as_str()));

        let (date, subject, description) = correction();
        h.pipeline
            .resubmit(&ledger_key, date, subject, description)
            .await
            .unwrap();

        let corrected: Manifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(corrected.status, "ok");
        assert_eq!(corrected.manifest_id, ledger_key);
        assert_eq!(corrected.sha256, content_sha);
        assert_eq!(
            corrected.duplicate_of.as_deref(),
            Some(content_sha.as_str())
        );
        assert_eq!(std::fs::read(&source).unwrap(), body);
        assert!(!quarantined.exists());
        assert_eq!(
            h.pipeline.ledger.get(&ledger_key).unwrap().unwrap().state,
            JobState::Emitted
        );
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

    #[tokio::test]
    async fn local_resubmit_after_settings_switch_commits_only_to_the_pinned_local_root() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("pinned-local");
        });
        let rel = "clients/acme/invoice.pdf";
        let (sha, path) = h.seed(rel, "local correction bytes");
        h.pipeline
            .flag(&sha, &path, "SLM_FAIL:needs review".into(), &clock())
            .await;
        let flagged = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let quarantined = PathBuf::from(flagged.quarantine_path.unwrap());
        let expected_bytes = std::fs::read(&quarantined).unwrap();
        let pinned_local = h.pipeline.cfg.local_output_dir.clone();
        let mut switched_cfg = h.pipeline.cfg.clone();
        switched_cfg.output_mode = crate::config::OutputMode::PowerAutomate;
        switched_cfg.outbox_dir = h.dir.path().join("new-pa-outbox");
        std::fs::create_dir_all(&switched_cfg.outbox_dir).unwrap();
        let restarted = h.restarted_with(switched_cfg.clone());

        let (date, subject, description) = correction();
        restarted
            .resubmit(&sha, date, subject, description)
            .await
            .unwrap();

        let mid = manifest_id(&sha, rel);
        let receipt = local_output::read_receipt(&pinned_local, &mid)
            .unwrap()
            .expect("pinned Local root receives the correction receipt");
        assert_eq!(receipt.manifest.status, "ok");
        let output = pinned_local.join(
            receipt
                .manifest
                .new_filename
                .as_deref()
                .expect("ok local receipt names its output"),
        );
        assert_eq!(std::fs::read(&output).unwrap(), expected_bytes);
        assert!(
            !switched_cfg
                .manifests_dir()
                .join(format!("{mid}.json"))
                .exists(),
            "a stopped Settings switch must not redirect correction to PA"
        );
        assert!(
            !quarantined.exists(),
            "quarantine is removed only after the local output and receipt commit"
        );
        assert_eq!(
            restarted.ledger.get(&sha).unwrap().unwrap().state,
            JobState::Emitted
        );

        let receipt_json = serde_json::to_vec(&receipt).unwrap();
        assert!(
            restarted
                .resubmit(&sha, correction().0, correction().1, correction().2)
                .await
                .is_err(),
            "a stale second correction must not create another delivery"
        );
        assert_eq!(
            serde_json::to_vec(
                &local_output::read_receipt(&pinned_local, &mid)
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            receipt_json
        );
        assert_eq!(std::fs::read(&output).unwrap(), expected_bytes);
    }

    #[tokio::test]
    async fn local_review_correct_lease_blocks_dismiss_and_commits_one_output() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "review/correct-wins.pdf";
        let (sha, source) = h.seed(rel, "correct winner bytes");
        h.pipeline
            .flag(&sha, &source, "SLM_FAIL:review".into(), &clock())
            .await;
        let owner = h
            .pipeline
            .ledger
            .begin_review_operation(&sha, "correct")
            .unwrap()
            .unwrap();
        let state = h.app_state();
        assert!(crate::dismiss_inner(&state, &sha, "losing dismissal").is_err());
        let (date, subject, description) = correction();
        h.pipeline
            .resubmit_with_owner(&sha, date, subject, description, Some(owner))
            .await
            .unwrap();
        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(job.state, JobState::Emitted);
        assert!(job.quarantine_path.is_none());
        let receipt =
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &job.delivery_id)
                .unwrap()
                .unwrap();
        assert_eq!(receipt.manifest.status, "ok");
        assert_eq!(
            std::fs::read_dir(&h.pipeline.cfg.local_output_dir)
                .unwrap()
                .flatten()
                .filter(|entry| entry.path().is_file())
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn local_review_dismiss_lease_blocks_correction_and_retains_quarantine() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let rel = "review/dismiss-wins.pdf";
        let (sha, source) = h.seed(rel, "dismiss winner bytes");
        h.pipeline
            .flag(&sha, &source, "SLM_FAIL:review".into(), &clock())
            .await;
        let before = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let quarantined = PathBuf::from(before.quarantine_path.as_ref().unwrap());
        let owner = h
            .pipeline
            .ledger
            .begin_review_operation(&sha, "dismiss")
            .unwrap()
            .unwrap();
        let (date, subject, description) = correction();
        assert!(h
            .pipeline
            .resubmit(&sha, date, subject, description)
            .await
            .is_err());
        let state = h.app_state();
        assert!(h
            .pipeline
            .ledger
            .release_review_operation(&sha, "dismiss", &owner)
            .unwrap());
        crate::dismiss_inner(&state, &sha, "dismiss winner").unwrap();

        let job = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        assert_eq!(job.state, JobState::Dismissed);
        assert!(quarantined.exists());
        assert_eq!(job.quarantine_path.as_deref(), quarantined.to_str());
        let receipt =
            local_output::read_receipt(&h.pipeline.cfg.local_output_dir, &job.delivery_id)
                .unwrap()
                .unwrap();
        assert_eq!(receipt.manifest.status, "dismissed");
        assert_eq!(
            std::fs::read_dir(&h.pipeline.cfg.local_output_dir)
                .unwrap()
                .flatten()
                .filter(|entry| entry.path().is_file())
                .count(),
            0,
            "dismissal must not leave an orphan Local output"
        );
    }

    #[tokio::test]
    async fn startup_releases_review_lease_abandoned_before_terminal_artifact() {
        let h = Harness::with(|cfg| {
            cfg.output_mode = crate::config::OutputMode::Local;
            cfg.local_output_dir = cfg.processing_dir.parent().unwrap().join("local-output");
        });
        let (sha, source) = h.seed("review/abandoned.pdf", "abandoned lease bytes");
        h.pipeline
            .flag(&sha, &source, "SLM_FAIL:review".into(), &clock())
            .await;
        assert!(h
            .pipeline
            .ledger
            .begin_review_operation(&sha, "correct")
            .unwrap()
            .is_some());
        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0
        );
        assert!(h
            .pipeline
            .ledger
            .begin_review_operation(&sha, "dismiss")
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn pa_resubmit_after_settings_switch_replaces_only_the_pinned_pa_manifest() {
        let h = Harness::new();
        let rel = "clients/acme/invoice.pdf";
        let (sha, path) = h.seed(rel, "pa correction bytes");
        h.pipeline
            .flag(&sha, &path, "SLM_FAIL:needs review".into(), &clock())
            .await;
        let flagged = h.pipeline.ledger.get(&sha).unwrap().unwrap();
        let quarantined = PathBuf::from(flagged.quarantine_path.unwrap());
        let pinned_outbox = h.pipeline.cfg.outbox_dir.clone();
        let mut switched_cfg = h.pipeline.cfg.clone();
        switched_cfg.output_mode = crate::config::OutputMode::Local;
        switched_cfg.local_output_dir = h.dir.path().join("new-local-output");
        std::fs::create_dir_all(&switched_cfg.local_output_dir).unwrap();
        let restarted = h.restarted_with(switched_cfg.clone());

        let (date, subject, description) = correction();
        restarted
            .resubmit(&sha, date, subject, description)
            .await
            .unwrap();

        let mid = manifest_id(&sha, rel);
        let pinned_manifest = pinned_outbox.join("_manifests").join(format!("{mid}.json"));
        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(&pinned_manifest).unwrap()).unwrap();
        assert_eq!(manifest.status, "ok");
        assert!(
            local_output::read_receipt(&switched_cfg.local_output_dir, &mid)
                .unwrap()
                .is_none(),
            "a PA row must never create a Local receipt after Settings switch"
        );
        assert!(
            std::fs::read_dir(&switched_cfg.local_output_dir)
                .unwrap()
                .next()
                .is_none(),
            "a PA correction must leave the new Local root empty"
        );
        assert!(!quarantined.exists());
        assert!(
            path.exists(),
            "PA correction restores the review file for Flow 2"
        );
        assert_eq!(
            restarted.ledger.get(&sha).unwrap().unwrap().state,
            JobState::Emitted
        );

        let manifest_json = std::fs::read(&pinned_manifest).unwrap();
        assert!(
            restarted
                .resubmit(&sha, correction().0, correction().1, correction().2)
                .await
                .is_err(),
            "a stale PA correction must not replace the committed manifest"
        );
        assert_eq!(std::fs::read(&pinned_manifest).unwrap(), manifest_json);
        assert!(
            local_output::read_receipt(&switched_cfg.local_output_dir, &mid)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn pa_review_lease_allows_exactly_one_correction_or_dismissal() {
        let correction_wins = Harness::new();
        let rel = "review/pa-correct.pdf";
        let (sha, source) = correction_wins.seed(rel, "pa correct bytes");
        correction_wins
            .pipeline
            .flag(&sha, &source, "SLM_FAIL:review".into(), &clock())
            .await;
        let owner = correction_wins
            .pipeline
            .ledger
            .begin_review_operation(&sha, "correct")
            .unwrap()
            .unwrap();
        assert!(crate::dismiss_inner(&correction_wins.app_state(), &sha, "loser").is_err());
        let (date, subject, description) = correction();
        correction_wins
            .pipeline
            .resubmit_with_owner(&sha, date, subject, description, Some(owner))
            .await
            .unwrap();
        assert_eq!(
            correction_wins
                .pipeline
                .ledger
                .get(&sha)
                .unwrap()
                .unwrap()
                .state,
            JobState::Emitted
        );
        assert_eq!(correction_wins.manifest(&sha, rel).unwrap().status, "ok");
        assert!(source.exists());

        let dismissal_wins = Harness::new();
        let rel = "review/pa-dismiss.pdf";
        let (sha, source) = dismissal_wins.seed(rel, "pa dismiss bytes");
        dismissal_wins
            .pipeline
            .flag(&sha, &source, "SLM_FAIL:review".into(), &clock())
            .await;
        let quarantined = PathBuf::from(
            dismissal_wins
                .pipeline
                .ledger
                .get(&sha)
                .unwrap()
                .unwrap()
                .quarantine_path
                .unwrap(),
        );
        let owner = dismissal_wins
            .pipeline
            .ledger
            .begin_review_operation(&sha, "dismiss")
            .unwrap()
            .unwrap();
        let (date, subject, description) = correction();
        assert!(dismissal_wins
            .pipeline
            .resubmit(&sha, date, subject, description)
            .await
            .is_err());
        assert!(dismissal_wins
            .pipeline
            .ledger
            .release_review_operation(&sha, "dismiss", &owner)
            .unwrap());
        crate::dismiss_inner(&dismissal_wins.app_state(), &sha, "winner").unwrap();
        assert_eq!(
            dismissal_wins
                .pipeline
                .ledger
                .get(&sha)
                .unwrap()
                .unwrap()
                .state,
            JobState::Dismissed
        );
        assert_eq!(
            dismissal_wins.manifest(&sha, rel).unwrap().status,
            "dismissed"
        );
        assert!(quarantined.exists());
        assert!(!source.exists());
    }

    #[tokio::test]
    async fn legacy_pa_flagged_row_is_pinned_before_settings_switch_and_corrects_only_to_that_outbox(
    ) {
        let h = Harness::new();
        let rel = "legacy/invoice.pdf";
        let source = h.pipeline.cfg.processing_dir.join(rel);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"legacy flagged correction bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        // `ingest` models a row opened from the pre-delivery-root schema:
        // mode defaults to PA and root remains empty until this one-way pin.
        h.pipeline
            .ledger
            .ingest(&sha, &source.to_string_lossy(), "invoice.pdf", rel, "pdf")
            .unwrap();
        let quarantined = h.pipeline.cfg.quarantine_dir.join("legacy-invoice.pdf");
        std::fs::rename(&source, &quarantined).unwrap();
        h.pipeline
            .ledger
            .update_fields(
                &sha,
                &[
                    ("flag_reason", Some("SLM_FAIL:legacy review".into())),
                    (
                        "quarantine_path",
                        Some(quarantined.to_string_lossy().into_owned()),
                    ),
                ],
            )
            .unwrap();
        h.pipeline
            .ledger
            .set_state(&sha, JobState::Flagged)
            .unwrap();

        let pinned_outbox = h.pipeline.cfg.outbox_dir.clone();
        assert_eq!(
            reconcile_terminal_manifests(&h.pipeline.cfg, &h.pipeline.ledger).unwrap(),
            0,
            "startup pins an unresolved legacy row even without a newer manifest"
        );
        assert_eq!(
            h.pipeline.ledger.get(&sha).unwrap().unwrap().delivery_root,
            pinned_outbox.to_string_lossy()
        );

        let mut switched_cfg = h.pipeline.cfg.clone();
        switched_cfg.output_mode = crate::config::OutputMode::Local;
        switched_cfg.outbox_dir = h.dir.path().join("outbox-b");
        switched_cfg.local_output_dir = h.dir.path().join("local-b");
        std::fs::create_dir_all(&switched_cfg.outbox_dir).unwrap();
        std::fs::create_dir_all(&switched_cfg.local_output_dir).unwrap();
        let restarted = h.restarted_with(switched_cfg.clone());
        let (date, subject, description) = correction();
        restarted
            .resubmit(&sha, date, subject, description)
            .await
            .unwrap();

        let mid = manifest_id(&sha, rel);
        let pinned_path = pinned_outbox.join("_manifests").join(format!("{mid}.json"));
        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(&pinned_path).unwrap()).unwrap();
        assert_eq!(manifest.status, "ok");
        assert!(!switched_cfg
            .manifests_dir()
            .join(format!("{mid}.json"))
            .exists());
        assert!(
            local_output::read_receipt(&switched_cfg.local_output_dir, &mid)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            restarted.ledger.get(&sha).unwrap().unwrap().state,
            JobState::Emitted
        );
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

    #[test]
    fn evidence_trace_round_trips_and_metric_event_contains_no_source_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut trace = filter::EvidenceTrace {
            routing: "semantic".into(),
            semantic_available: true,
            entity_available: true,
            source_paragraphs: 10,
            selected_paragraphs: 3,
            compression: filter::CompressionMetrics {
                source_chars: 1_000,
                source_tokens_approx: 250,
                bundle_chars: 400,
                bundle_tokens_approx: 100,
                saved_chars: 600,
                savings_ratio: 0.6,
            },
            ..Default::default()
        };
        trace
            .ranked_paragraphs
            .push(crate::sidecar::RankedParagraph {
                index: 7,
                text: "Alice Example signed the confidential agreement".into(),
                start_char: 80,
                end_char: 127,
                score: 0.91,
                probe: "parties to this document".into(),
                rank: 0,
            });
        trace.entities.push(crate::sidecar::EntitySpan {
            label: "PERSON".into(),
            text: "Alice Example".into(),
            score: 0.95,
            paragraph_index: 7,
            start_char: 80,
            end_char: 93,
            iso: None,
        });

        let path = write_evidence_trace(dir.path(), "abc123", &trace).unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("abc123.evidence.json")
        );
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(stored["routing"], "semantic");
        assert_eq!(stored["compression"]["saved_chars"], 600);
        assert_eq!(stored["ranked_paragraphs"][0]["index"], 7);
        assert_eq!(stored["entities"][0]["text"], "Alice Example");

        trace.routing = "bypass_source_fits".into();
        write_evidence_trace(dir.path(), "abc123", &trace).unwrap();
        let replaced: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("abc123.evidence.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(replaced["routing"], "bypass_source_fits");
        assert!(
            !dir.path().join("abc123.evidence.json.bak").exists(),
            "a successful replacement must not retain a second source-text copy"
        );

        trace.routing = "semantic".into();
        let metric = evidence_metric_detail(&trace);
        assert!(metric.contains("routing=semantic"));
        assert!(metric.contains("source_chars=1000"));
        assert!(metric.contains("bundle_chars=400"));
        assert!(metric.contains("savings_permille=600"));
        assert!(metric.contains("paragraphs=3/10"));
        assert!(
            !metric.contains("Alice") && !metric.contains("confidential agreement"),
            "the encrypted ledger event must contain metrics, never source text"
        );
    }

    #[test]
    fn cache_purge_removes_markdown_and_trace_together() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("abc.md"), "raw document text").unwrap();
        std::fs::write(dir.path().join("abc.evidence.json"), "{}").unwrap();

        purge_cache_artifacts(dir.path(), "abc");

        assert!(!dir.path().join("abc.md").exists());
        assert!(!dir.path().join("abc.evidence.json").exists());
    }

    #[test]
    fn cache_retention_preserves_markdown_and_trace_together() {
        let h = Harness::with(|cfg| cfg.retain_cache = true);
        let cache = &h.pipeline.cfg.cache_dir;
        std::fs::write(cache.join("abc.md"), "raw document text").unwrap();
        std::fs::write(cache.join("abc.evidence.json"), "{}").unwrap();

        h.pipeline.purge_cache("abc");

        assert!(cache.join("abc.md").exists());
        assert!(cache.join("abc.evidence.json").exists());
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
            std::fs::write(cache.join(format!("{sha}.evidence.json")), "{}").unwrap();
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
            cache.join("1111.md").exists() && cache.join("1111.evidence.json").exists(),
            "flagged evidence and its trace must survive their TTL"
        );
        assert!(
            cache.join("2222.md").exists() && cache.join("2222.evidence.json").exists(),
            "an in-flight job's text and trace must survive"
        );
        assert!(
            !cache.join("3333.md").exists() && !cache.join("3333.evidence.json").exists(),
            "a delivered job's text and trace must go"
        );
        assert!(
            !cache.join("4444.md").exists() && !cache.join("4444.evidence.json").exists(),
            "a genuine orphan's text and trace must go"
        );
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

    /// An Evidence carrying `body` and, when `salient` is non-empty, exact
    /// selected text that stands in for the structured ranker in ladder tests.
    fn evidence_for(body: &str, salient: &[&str]) -> Evidence {
        let paragraphs = filter::segment_paragraphs(body);
        let ranked_paragraphs = salient
            .iter()
            .enumerate()
            .map(|(rank, text)| crate::sidecar::RankedParagraph {
                index: paragraphs.len() + rank,
                text: (*text).to_string(),
                start_char: 0,
                end_char: text.chars().count(),
                score: 0.8,
                probe: "test evidence".into(),
                rank: rank + 1,
            })
            .collect();
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
            paragraphs,
            ranked_paragraphs,
            entities: Vec::new(),
            semantic_lane_char_budget: 2_000,
            trace: crate::filter::EvidenceTrace::default(),
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

    /// Structured ranking is most important when deterministic harvesting is
    /// thin. Widening must keep those exact selected paragraphs while adding
    /// more source context, never replace them with a different summary.
    #[test]
    fn a_thin_documents_widened_bundle_keeps_its_ranked_paragraphs() {
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
            wide.contains("RANKED BODY PARAGRAPHS (exact source text):"),
            "the exact selected-text lane must survive the widening: {wide}"
        );
        assert!(wide.contains("continues until either party gives notice"));
    }

    /// Evidence budgets are measured in Unicode characters rather than UTF-8
    /// bytes, so multibyte legal names and currency symbols cannot be split.
    #[test]
    fn a_unicode_character_budget_never_splits_a_codepoint() {
        let body = format!("SUBJECT: Reçu\n\n{}", "€".repeat(4000));
        let ev = evidence_for(&body, &[]);
        let budget = 120;
        let cut = filter::widened_bundle(&ev, budget);

        assert!(cut.is_char_boundary(cut.len()));
        assert!(cut.chars().count() <= budget * 4);
        assert!(cut.contains("Reçu"));
    }
}
