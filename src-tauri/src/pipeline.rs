//! The orchestrator. Owns worker pools, drives each physical file instance
//! through the content pipeline, and emits replay-safe manifest v2 handoffs.

use crate::checker::{fs_metadata_dates, CheckError, Checker};
use crate::config::Config;
use crate::filter::{self, Evidence};
use crate::identity::{instance_id as derive_instance_id, normalize_relpath};
use crate::ledger::{InstanceState, Job, JobState, Ledger};
use crate::manifest::{write_manifest, Manifest, Pacer, MANIFEST_SCHEMA_VERSION};
use crate::routing::{self, Route};
use crate::sidecar::{ConvertResult, Sidecar};
use crate::slm::{SlmLane, Tier};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tauri::Emitter;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

#[derive(Debug, Clone)]
struct FileContext {
    path: PathBuf,
    sha256: String,
    instance_id: String,
    original_name: String,
    original_relpath: String,
    ext: String,
}

pub struct Pipeline {
    pub cfg: Config,
    pub ledger: Arc<Ledger>,
    pub sidecar: Arc<Sidecar>,
    pub slm: Arc<SlmLane>,
    pub app: tauri::AppHandle,
    pub paused: Arc<AtomicBool>,
    convert_slots: Arc<Semaphore>,
    slm_slots: Arc<Semaphore>,
    pacer: Arc<Pacer>,
    model_versions: serde_json::Value,
    content_locks: StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

const PDF_TEXT_MEDIAN_CHARS: u64 = 200;
const OCR_CONF_FLOOR: f64 = 0.55;

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
            slm_slots: Arc::new(Semaphore::new(4)),
            pacer: Arc::new(Pacer::new(cfg.manifest_emit_per_min)),
            cfg,
            ledger,
            sidecar,
            slm,
            app,
            paused: Arc::new(AtomicBool::new(false)),
            model_versions,
            content_locks: StdMutex::new(HashMap::new()),
        })
    }

    fn emit_update(&self, sha: &str) {
        if let Ok(Some(job)) = self.ledger.get(sha) {
            let _ = self.app.emit("job-updated", &job);
        }
    }

    fn content_lock(&self, sha: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.content_locks.lock().unwrap();
        locks
            .entry(sha.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Entry point per discovered file. Spawned as a task; bounded by pools.
    pub async fn process_file(self: Arc<Self>, path: PathBuf) {
        if self.paused.load(Ordering::Relaxed) {
            return;
        }
        let overall = tokio::time::timeout(
            std::time::Duration::from_secs(self.cfg.per_file_wall_clock_secs * 3),
            self.clone().process_inner(path.clone()),
        )
        .await;
        if overall.is_err() {
            log::error!("wall-clock cap blown for {path:?}");
        }
    }

    async fn process_inner(self: Arc<Self>, path: PathBuf) {
        // ---- Physical instance registration ---------------------------------
        let sha256 = match hash_file(&path) {
            Ok(hash) => hash,
            Err(error) => {
                log::warn!(
                    "hash failed for {path:?}: {error} (sync race? will retry on next event)"
                );
                return;
            }
        };
        let original_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
        let ext = routing::extension_of(&path);
        let original_relpath = relpath(&self.cfg.processing_dir, &path);
        let instance_id = derive_instance_id(&sha256, &normalize_relpath(&original_relpath));
        let context = FileContext {
            path: path.clone(),
            sha256: sha256.clone(),
            instance_id,
            original_name,
            original_relpath,
            ext,
        };

        match self.ledger.register_instance(
            &context.instance_id,
            &context.sha256,
            &context.path.to_string_lossy(),
            &context.original_name,
            &context.ext,
        ) {
            Ok(Some(existing)) if existing.state.is_terminal() => return,
            Ok(_) => {}
            Err(error) => {
                log::error!("file-instance registration failed: {error}");
                return;
            }
        }

        if self.recover_existing_manifest(&context) {
            return;
        }
        if let Err(error) = self
            .ledger
            .set_instance_state(&context.instance_id, InstanceState::Processing)
        {
            log::error!("cannot mark instance processing: {error}");
            return;
        }

        // Only one physical copy performs or resumes content-level work. Other
        // copies wait, then reuse the accepted content metadata.
        let content_lock = self.content_lock(&context.sha256);
        let _content_guard = content_lock.lock().await;
        if self
            .ledger
            .instance(&context.instance_id)
            .ok()
            .flatten()
            .is_some_and(|instance| instance.state.is_terminal())
        {
            return;
        }
        if self.recover_existing_manifest(&context) {
            return;
        }

        let (content_owner_id, content_owner_ext) = match self.ledger.ingest(
            &context.sha256,
            &context.path.to_string_lossy(),
            &context.original_name,
            &context.ext,
        ) {
            Ok(None) => (context.instance_id.clone(), context.ext.clone()),
            Ok(Some(existing)) => {
                if existing.state == JobState::Emitted {
                    if let Err(error) = self.emit_from_existing(&context, &existing).await {
                        let duplicate = !same_path(&existing.original_path, &context.path);
                        self.flag(
                            &context,
                            format!("RUNTIME_FAIL:cached metadata {error}"),
                            false,
                            duplicate,
                        )
                        .await;
                    }
                    return;
                }
                if existing.state == JobState::Flagged {
                    let duplicate = !same_path(&existing.original_path, &context.path);
                    let reason = existing
                        .flag_reason
                        .clone()
                        .unwrap_or_else(|| "PREVIOUSLY_FLAGGED:content job".into());
                    self.flag(&context, reason, false, duplicate).await;
                    return;
                }
                let owner_id = match self.ensure_content_owner_instance(&existing) {
                    Ok(owner_id) => owner_id,
                    Err(error) => {
                        log::error!("cannot restore content owner instance: {error}");
                        context.instance_id.clone()
                    }
                };
                (owner_id, existing.ext.clone())
            }
            Err(error) => {
                log::error!("ledger ingest failed: {error}");
                return;
            }
        };
        let is_duplicate_instance = content_owner_id != context.instance_id;

        let _ = self.ledger.log_event(
            &context.sha256,
            "ingest",
            &format!(
                "instance={} path={}",
                context.instance_id,
                context.path.display()
            ),
        );
        self.emit_update(&context.sha256);

        // ---- Route ---------------------------------------------------------
        let decision = routing::detect(&context.path);
        let _ = self.ledger.update_fields(
            &context.sha256,
            &[("detected_type", Some(decision.detected_type.clone()))],
        );
        if decision.route == Route::Flag {
            self.flag(
                &context,
                decision
                    .flag_reason
                    .unwrap_or_else(|| "UNSUPPORTED".into()),
                true,
                is_duplicate_instance,
            )
            .await;
            return;
        }

        // PDF text-layer probe decides native vs scanned.
        let mut route = decision.route;
        if decision.detected_type == "application/pdf" {
            match self.sidecar.pdf_probe(&context.path.to_string_lossy()) {
                Ok((median, _pages)) => {
                    if median < PDF_TEXT_MEDIAN_CHARS {
                        route = Route::Scanned;
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    if message.contains("password") || message.contains("encrypted") {
                        self.flag(
                            &context,
                            "ENCRYPTED:password protected".into(),
                            true,
                            is_duplicate_instance,
                        )
                        .await;
                        return;
                    }
                }
            }
        }
        let _ = self.ledger.update_fields(
            &context.sha256,
            &[("route", Some(format!("{route:?}").to_lowercase()))],
        );

        // ---- Convert (retry ladder row 1-2) -------------------------------
        let conversion = {
            let _permit = self.convert_slots.acquire().await.unwrap();
            self.convert_with_retries(&context, route, is_duplicate_instance)
                .await
        };
        let conversion = match conversion {
            Some(conversion) => conversion,
            None => return,
        };
        if conversion.encrypted {
            self.flag(
                &context,
                "ENCRYPTED:password protected".into(),
                true,
                is_duplicate_instance,
            )
            .await;
            return;
        }
        if conversion.markdown.trim().len() < 30 {
            self.flag(
                &context,
                "CONVERT_FAIL:empty extraction".into(),
                true,
                is_duplicate_instance,
            )
            .await;
            return;
        }

        let mut extra_soft: Vec<String> = Vec::new();
        if conversion.letterhead_resets >= 2 {
            extra_soft.push("POSSIBLE_MULTIDOC".into());
        }
        let _ = self
            .ledger
            .set_state(&context.sha256, JobState::Converted);
        self.emit_update(&context.sha256);

        // Cache markdown for the review pane and Ettin training.
        let cache = self
            .cfg
            .cache_dir
            .join(format!("{}.md", context.sha256));
        let _ = std::fs::create_dir_all(&self.cfg.cache_dir);
        let _ = std::fs::write(&cache, &conversion.markdown);

        // ---- Filter --------------------------------------------------------
        let ettin_enabled = !self.cfg.ettin_model_dir.is_empty();
        let filtered = match filter::build_evidence(
            &self.sidecar,
            &conversion.markdown,
            conversion.doc_meta_dates.clone(),
            ettin_enabled,
            self.cfg.evidence_token_budget,
        ) {
            Ok(filtered) => filtered,
            Err(error) => {
                self.flag(
                    &context,
                    format!("RUNTIME_FAIL:filter {error}"),
                    true,
                    is_duplicate_instance,
                )
                .await;
                return;
            }
        };
        let evidence = filtered.evidence;
        let _ = self.ledger.update_fields(
            &context.sha256,
            &[
                ("doc_type", Some(evidence.doc_type.clone())),
                ("language", Some(evidence.language.clone())),
            ],
        );
        let _ = self
            .ledger
            .set_state(&context.sha256, JobState::Filtered);
        self.emit_update(&context.sha256);

        // ---- Name + Validate (retry ladder rows 3-5) ----------------------
        let (fs_dates, modified_iso) = fs_metadata_dates(&context.path);
        let mut meta_dates = filtered.doc_meta_dates;
        meta_dates.extend(fs_dates);
        meta_dates.dedup();

        let checker = Checker::new(self.cfg.max_filename_len);
        let ettin_date: Option<String> = evidence
            .ettin_spans
            .iter()
            .filter(|span| span.label == "DATE")
            .max_by(|left, right| left.score.total_cmp(&right.score))
            .and_then(|span| span.iso.clone());

        let validated = {
            let _permit = self.slm_slots.acquire().await.unwrap();
            self.name_with_retries(
                &context.sha256,
                &evidence,
                &checker,
                &meta_dates,
                &modified_iso,
                ettin_date.as_deref(),
            )
            .await
        };
        let mut validated = match validated {
            Some(validated) => validated,
            None => {
                self.flag(
                    &context,
                    "SLM_FAIL:no valid output after escalation".into(),
                    true,
                    is_duplicate_instance,
                )
                .await;
                return;
            }
        };
        validated.soft_flags.extend(extra_soft);
        let _ = self
            .ledger
            .set_state(&context.sha256, JobState::Validated);

        // Reserve the content owner's canonical filename first. If a different
        // physical copy resumed the work, it receives the first duplicate suffix
        // while the original owner keeps the canonical reservation.
        let owner_final_filename = match self.ledger.reserve_filename(
            &content_owner_id,
            &validated.base_name,
            &content_owner_ext,
        ) {
            Ok(filename) => filename,
            Err(error) => {
                self.flag(
                    &context,
                    format!("RUNTIME_FAIL:reserve {error}"),
                    true,
                    is_duplicate_instance,
                )
                .await;
                return;
            }
        };
        let final_filename = if is_duplicate_instance {
            match self.ledger.reserve_filename(
                &context.instance_id,
                &validated.base_name,
                &context.ext,
            ) {
                Ok(filename) => filename,
                Err(error) => {
                    self.flag(
                        &context,
                        format!("RUNTIME_FAIL:reserve duplicate {error}"),
                        true,
                        true,
                    )
                    .await;
                    return;
                }
            }
        } else {
            owner_final_filename.clone()
        };

        let content_soft_flags = validated.soft_flags.clone();
        let _ = self.ledger.update_fields(
            &context.sha256,
            &[
                ("proposed_date", Some(validated.date_iso.clone())),
                ("date_source", Some(validated.date_source.clone())),
                ("proposed_subject", Some(validated.subject.clone())),
                ("description", Some(validated.description.clone())),
                ("final_filename", Some(owner_final_filename)),
                ("soft_flags", Some(content_soft_flags.join(","))),
                ("model_versions", Some(self.model_versions.to_string())),
            ],
        );

        let mut instance_soft_flags = validated.soft_flags;
        if is_duplicate_instance
            && !instance_soft_flags
                .iter()
                .any(|flag| flag == "DUPLICATE_CONTENT")
        {
            instance_soft_flags.push("DUPLICATE_CONTENT".into());
        }

        self.pacer.permit().await;
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: context.instance_id.clone(),
            sha256: context.sha256.clone(),
            status: "ok".into(),
            original_name: context.original_name.clone(),
            original_relpath: context.original_relpath.clone(),
            new_filename: Some(final_filename),
            description: Some(validated.description),
            date: Some(validated.date_iso),
            date_source: Some(validated.date_source),
            doc_type: Some(evidence.doc_type),
            language: Some(evidence.language),
            duplicate_of: is_duplicate_instance.then(|| context.sha256.clone()),
            soft_flags: instance_soft_flags,
            flag_reason: None,
            model_versions: self.model_versions.clone(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        match write_manifest(&self.cfg.manifests_dir(), &manifest) {
            Ok(_) => {
                let _ = self
                    .ledger
                    .set_state(&context.sha256, JobState::Emitted);
                let _ = self
                    .ledger
                    .set_instance_state(&context.instance_id, InstanceState::Emitted);
                let _ = self.ledger.log_event(
                    &context.sha256,
                    "emit",
                    &format!("manifest {} written", context.instance_id),
                );
            }
            Err(error) => {
                let _ = self.ledger.log_event(
                    &context.sha256,
                    "emit",
                    &format!("manifest write failed: {error}"),
                );
                log::error!(
                    "manifest write failed for instance {}: {error}",
                    context.instance_id
                );
                return;
            }
        }
        self.emit_update(&context.sha256);
    }

    fn ensure_content_owner_instance(&self, job: &Job) -> anyhow::Result<String> {
        let owner_path = PathBuf::from(&job.original_path);
        let owner_relpath = relpath(&self.cfg.processing_dir, &owner_path);
        let owner_id = derive_instance_id(&job.sha256, &normalize_relpath(&owner_relpath));
        self.ledger.register_instance(
            &owner_id,
            &job.sha256,
            &job.original_path,
            &job.original_name,
            &job.ext,
        )?;
        Ok(owner_id)
    }

    fn recover_existing_manifest(&self, context: &FileContext) -> bool {
        let path = self
            .cfg
            .manifests_dir()
            .join(format!("{}.json", context.instance_id));
        if !path.exists() {
            return false;
        }
        let manifest: Manifest = match std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(anyhow::Error::from))
        {
            Ok(manifest) => manifest,
            Err(error) => {
                log::error!("cannot recover existing manifest {}: {error}", path.display());
                return false;
            }
        };
        if manifest.validate().is_err()
            || manifest.manifest_id != context.instance_id
            || manifest.sha256 != context.sha256
        {
            log::error!("existing manifest does not match instance {}", context.instance_id);
            return false;
        }

        let instance_state = if manifest.status == "ok" {
            let _ = self
                .ledger
                .set_state(&context.sha256, JobState::Emitted);
            InstanceState::Emitted
        } else {
            if let Some(reason) = manifest.flag_reason.clone() {
                let _ = self
                    .ledger
                    .update_fields(&context.sha256, &[("flag_reason", Some(reason))]);
            }
            let _ = self
                .ledger
                .set_state(&context.sha256, JobState::Flagged);
            InstanceState::Flagged
        };
        match self
            .ledger
            .set_instance_state(&context.instance_id, instance_state)
        {
            Ok(()) => true,
            Err(error) => {
                log::error!("cannot recover instance state from manifest: {error}");
                false
            }
        }
    }

    async fn convert_with_retries(
        &self,
        context: &FileContext,
        route: Route,
        duplicate: bool,
    ) -> Option<ConvertResult> {
        let path = context.path.to_string_lossy().to_string();
        let (head_pages, tail_pages) = (self.cfg.max_head_pages, self.cfg.max_tail_pages);
        for attempt in 1..=self.cfg.max_stage_attempts {
            let result = match (route, attempt) {
                (Route::Native, 1 | 2) => {
                    self.sidecar.convert(&path, head_pages, tail_pages)
                }
                (Route::Native, _) => self.sidecar.ocr(&path, 300, head_pages, tail_pages),
                (Route::Scanned, 1) => {
                    self.sidecar.ocr(&path, 300, head_pages, tail_pages)
                }
                (Route::Scanned, 2) => {
                    self.sidecar.ocr(&path, 400, head_pages, tail_pages)
                }
                (Route::Scanned, _) => self.sidecar.ocr(&path, 0, head_pages, tail_pages),
                (Route::Flag, _) => unreachable!(),
            };
            match result {
                Ok(conversion) => {
                    if conversion.ocr_used
                        && conversion.ocr_mean_conf < OCR_CONF_FLOOR
                        && attempt < self.cfg.max_stage_attempts
                    {
                        let _ = self.ledger.log_event(
                            &context.sha256,
                            "convert",
                            &format!(
                                "attempt {attempt}: ocr conf {:.2} below floor, escalating",
                                conversion.ocr_mean_conf
                            ),
                        );
                        continue;
                    }
                    return Some(conversion);
                }
                Err(error) => {
                    let _ = self.ledger.log_event(
                        &context.sha256,
                        "convert",
                        &format!("attempt {attempt} failed: {error}"),
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
            context,
            format!("{reason}:all conversion attempts exhausted"),
            true,
            duplicate,
        )
        .await;
        None
    }

    async fn name_with_retries(
        &self,
        sha: &str,
        evidence: &Evidence,
        checker: &Checker,
        meta_dates: &[String],
        modified_iso: &str,
        ettin_date: Option<&str>,
    ) -> Option<crate::checker::Validated> {
        let mut violation: Option<String> = None;
        for attempt in 1..=self.cfg.max_stage_attempts {
            let (tier, bundle) = match attempt {
                1 => (Tier::Primary, evidence.bundle.clone()),
                2 => (
                    Tier::Primary,
                    filter::trimmed_bundle(evidence, self.cfg.evidence_token_budget),
                ),
                _ => {
                    let mut bundle = evidence.bundle.clone();
                    let cap = self.cfg.evidence_token_budget * 8;
                    if bundle.len() > cap {
                        bundle.truncate(cap);
                    }
                    (Tier::Escalation, bundle)
                }
            };
            let output = self
                .slm
                .name_document(
                    tier,
                    &bundle,
                    &evidence.doc_type,
                    &evidence.language,
                    violation.as_deref(),
                )
                .await;
            match output {
                Ok(output) => match checker.check(
                    &output,
                    &evidence.harvest,
                    meta_dates,
                    modified_iso,
                    ettin_date,
                ) {
                    Ok(mut validated) => {
                        if validated
                            .soft_flags
                            .iter()
                            .any(|flag| flag.starts_with("SPAN_MISMATCH"))
                            && attempt == 1
                            && ettin_date.is_some()
                        {
                            violation = Some(format!(
                                "your date disagrees with a high-confidence extracted DATE span ({}); re-examine the evidence spans",
                                ettin_date.unwrap()
                            ));
                            let _ = self
                                .ledger
                                .log_event(sha, "name", "span mismatch, re-prompting");
                            let retry = self
                                .slm
                                .name_document(
                                    tier,
                                    &bundle,
                                    &evidence.doc_type,
                                    &evidence.language,
                                    violation.as_deref(),
                                )
                                .await;
                            if let Ok(second_output) = retry {
                                if let Ok(second_validated) = checker.check(
                                    &second_output,
                                    &evidence.harvest,
                                    meta_dates,
                                    modified_iso,
                                    ettin_date,
                                ) {
                                    if !second_validated
                                        .soft_flags
                                        .iter()
                                        .any(|flag| flag.starts_with("SPAN_MISMATCH"))
                                    {
                                        return Some(second_validated);
                                    }
                                }
                            }
                            validated
                                .soft_flags
                                .push("SPAN_MISMATCH_PERSISTED".into());
                        }
                        let _ = self.ledger.set_state(sha, JobState::Named);
                        return Some(validated);
                    }
                    Err(check_error) => {
                        violation = Some(check_error.to_string());
                        let _ = self.ledger.log_event(
                            sha,
                            "validate",
                            &format!("attempt {attempt} rejected: {check_error}"),
                        );
                        if matches!(check_error, CheckError::TooLong(_, _)) && attempt >= 2 {
                            violation = Some("subject too long; use at most 6 short words".into());
                        }
                    }
                },
                Err(error) => {
                    violation = None;
                    let _ = self.ledger.log_event(
                        sha,
                        "name",
                        &format!("attempt {attempt} SLM error: {error}"),
                    );
                }
            }
        }
        None
    }

    async fn emit_from_existing(
        &self,
        context: &FileContext,
        existing: &Job,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(existing.state == JobState::Emitted, "content job is not emitted");
        let original_final = existing
            .final_filename
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("missing accepted filename"))?;
        let base = base_without_extension(original_final);
        let final_filename = self
            .ledger
            .reserve_filename(&context.instance_id, base, &context.ext)?;
        let duplicate = !same_path(&existing.original_path, &context.path);

        let description = existing
            .description
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing accepted description"))?;
        let date = existing
            .proposed_date
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing accepted date"))?;
        let date_source = existing
            .date_source
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing accepted date source"))?;
        let mut soft_flags = parse_soft_flags(existing.soft_flags.as_deref());
        if duplicate && !soft_flags.iter().any(|flag| flag == "DUPLICATE_CONTENT") {
            soft_flags.push("DUPLICATE_CONTENT".into());
        }

        self.pacer.permit().await;
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: context.instance_id.clone(),
            sha256: context.sha256.clone(),
            status: "ok".into(),
            original_name: context.original_name.clone(),
            original_relpath: context.original_relpath.clone(),
            new_filename: Some(final_filename),
            description: Some(description),
            date: Some(date),
            date_source: Some(date_source),
            doc_type: existing.doc_type.clone(),
            language: existing.language.clone(),
            duplicate_of: duplicate.then(|| context.sha256.clone()),
            soft_flags,
            flag_reason: None,
            model_versions: model_versions_for_job(existing, &self.model_versions),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        write_manifest(&self.cfg.manifests_dir(), &manifest)?;
        self.ledger
            .set_instance_state(&context.instance_id, InstanceState::Emitted)?;
        self.ledger.log_event(
            &context.sha256,
            "emit",
            &format!(
                "reused content metadata for instance {} duplicate={duplicate}",
                context.instance_id
            ),
        )?;
        Ok(())
    }

    async fn flag(
        &self,
        context: &FileContext,
        reason: String,
        mark_content: bool,
        duplicate: bool,
    ) {
        self.pacer.permit().await;
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: context.instance_id.clone(),
            sha256: context.sha256.clone(),
            status: "flagged".into(),
            original_name: context.original_name.clone(),
            original_relpath: context.original_relpath.clone(),
            new_filename: None,
            description: None,
            date: None,
            date_source: None,
            doc_type: None,
            language: None,
            duplicate_of: duplicate.then(|| context.sha256.clone()),
            soft_flags: duplicate
                .then(|| vec!["DUPLICATE_CONTENT".into()])
                .unwrap_or_default(),
            flag_reason: Some(reason.clone()),
            model_versions: self
                .ledger
                .get(&context.sha256)
                .ok()
                .flatten()
                .as_ref()
                .map(|job| model_versions_for_job(job, &self.model_versions))
                .unwrap_or_else(|| self.model_versions.clone()),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };

        if let Err(error) = write_manifest(&self.cfg.manifests_dir(), &manifest) {
            let _ = self.ledger.log_event(
                &context.sha256,
                "flag",
                &format!("flagged manifest write failed: {error}"),
            );
            log::error!(
                "flagged manifest write failed for instance {}: {error}",
                context.instance_id
            );
            return;
        }

        if mark_content {
            let _ = self
                .ledger
                .update_fields(&context.sha256, &[("flag_reason", Some(reason.clone()))]);
            let _ = self
                .ledger
                .set_state(&context.sha256, JobState::Flagged);
        }
        let _ = self
            .ledger
            .set_instance_state(&context.instance_id, InstanceState::Flagged);
        let _ = self.ledger.log_event(
            &context.sha256,
            "flag",
            &format!("instance={} reason={reason}", context.instance_id),
        );

        // Only move after the manifest is durable. A failed move leaves a
        // reported, terminal instance rather than silently losing the source.
        let _ = std::fs::create_dir_all(&self.cfg.quarantine_dir);
        if context.path.exists() {
            let mut destination = self.cfg.quarantine_dir.join(&context.original_name);
            if destination.exists() {
                destination = self.cfg.quarantine_dir.join(format!(
                    "{}-{}",
                    &context.instance_id[..12],
                    context.original_name
                ));
            }
            if let Err(rename_error) = std::fs::rename(&context.path, &destination) {
                if let Err(copy_error) = std::fs::copy(&context.path, &destination) {
                    log::error!(
                        "cannot quarantine {}: rename={rename_error}; copy={copy_error}",
                        context.path.display()
                    );
                }
            }
        }

        self.emit_update(&context.sha256);
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
        let markdown = std::fs::read_to_string(self.cfg.cache_dir.join(format!("{sha}.md")))
            .unwrap_or_default();
        let harvest = crate::harvest::harvest(&markdown);
        let checker = Checker::new(self.cfg.max_filename_len);

        let output = crate::checker::SlmOutput {
            date: date.clone(),
            date_source: "document".into(),
            subject,
            description,
        };
        let today = chrono::Utc::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let validated = checker.check(&output, &harvest, &[date], &today, None)?;

        let original_path = PathBuf::from(&job.original_path);
        let stable_relpath = relpath(&self.cfg.processing_dir, &original_path);
        let instance_id = derive_instance_id(sha, &normalize_relpath(&stable_relpath));
        self.ledger.register_instance(
            &instance_id,
            sha,
            &job.original_path,
            &job.original_name,
            &job.ext,
        )?;
        self.ledger
            .set_instance_state(&instance_id, InstanceState::Processing)?;
        let final_filename = self
            .ledger
            .reserve_filename(&instance_id, &validated.base_name, &job.ext)?;

        self.ledger.update_fields(
            sha,
            &[
                ("proposed_date", Some(validated.date_iso.clone())),
                ("date_source", Some("human".into())),
                ("proposed_subject", Some(validated.subject.clone())),
                ("description", Some(validated.description.clone())),
                ("final_filename", Some(final_filename.clone())),
                ("flag_reason", None),
                ("soft_flags", Some("HUMAN_CORRECTED".into())),
            ],
        )?;
        self.ledger
            .log_event(sha, "resubmit", "human correction accepted")?;

        let restored_path = if original_path.starts_with(&self.cfg.processing_dir) {
            original_path
        } else {
            self.cfg.processing_dir.join(&job.original_name)
        };
        if let Some(parent) = restored_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !restored_path.exists() {
            let normal = self.cfg.quarantine_dir.join(&job.original_name);
            let prefixed = self.cfg.quarantine_dir.join(format!(
                "{}-{}",
                &instance_id[..12],
                job.original_name
            ));
            let quarantined = if normal.exists() { normal } else { prefixed };
            if quarantined.exists() {
                if std::fs::rename(&quarantined, &restored_path).is_err() {
                    std::fs::copy(&quarantined, &restored_path)?;
                }
            }
        }
        let current_relpath = relpath(&self.cfg.processing_dir, &restored_path);
        let model_versions = model_versions_for_job(&job, &self.model_versions);

        let manifest = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            manifest_id: instance_id.clone(),
            sha256: sha.to_string(),
            status: "ok".into(),
            original_name: job.original_name.clone(),
            original_relpath: current_relpath,
            new_filename: Some(final_filename),
            description: Some(validated.description),
            date: Some(validated.date_iso),
            date_source: Some("human".into()),
            doc_type: job.doc_type.clone(),
            language: job.language.clone(),
            duplicate_of: None,
            soft_flags: vec!["HUMAN_CORRECTED".into()],
            flag_reason: None,
            model_versions,
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        write_manifest(&self.cfg.manifests_dir(), &manifest)?;
        self.ledger.set_state(sha, JobState::Emitted)?;
        self.ledger
            .set_instance_state(&instance_id, InstanceState::Emitted)?;
        self.emit_update(sha);
        Ok(())
    }
}

fn model_versions_for_job(job: &Job, fallback: &serde_json::Value) -> serde_json::Value {
    job.model_versions
        .as_deref()
        .and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_else(|| fallback.clone())
}

fn parse_soft_flags(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|flag| !flag.is_empty())
        .map(str::to_string)
        .collect()
}

fn base_without_extension(filename: &str) -> &str {
    filename
        .rsplit_once('.')
        .map(|(base, _)| base)
        .unwrap_or(filename)
}

fn same_path(existing: &str, current: &Path) -> bool {
    normalize_relpath(existing) == normalize_relpath(&current.to_string_lossy())
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
