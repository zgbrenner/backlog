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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::Semaphore;

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
        })
    }

    fn emit_update(&self, sha: &str) {
        if let Ok(Some(job)) = self.ledger.get(sha) {
            let _ = self.app.emit("job-updated", &job);
        }
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
        // ---- Ingest --------------------------------------------------------
        let sha = match hash_file(&path) {
            Ok(h) => h,
            Err(e) => {
                log::warn!("hash failed for {path:?}: {e} (sync race? will retry on next event)");
                return;
            }
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string();
        let ext = routing::extension_of(&path);

        match self.ledger.ingest(&sha, &path.to_string_lossy(), &name, &ext) {
            Ok(None) => {} // new
            Ok(Some(existing)) => {
                match existing.state {
                    JobState::Emitted | JobState::Flagged => {
                        // Same content seen again under a new file: emit a
                        // duplicate manifest so PA can index " (2)".
                        if existing.original_path != path.to_string_lossy() {
                            self.handle_duplicate(&sha, &path, &name, &ext, &existing).await;
                        }
                        return;
                    }
                    _ => { /* resume mid-flight job below */ }
                }
            }
            Err(e) => {
                log::error!("ledger ingest failed: {e}");
                return;
            }
        }
        let _ = self.ledger.log_event(&sha, "ingest", &format!("path={}", path.display()));
        self.emit_update(&sha);

        // ---- Route ---------------------------------------------------------
        let decision = routing::detect(&path);
        let _ = self.ledger.update_fields(
            &sha,
            &[("detected_type", Some(decision.detected_type.clone()))],
        );
        if decision.route == Route::Flag {
            self.flag(&sha, &path, decision.flag_reason.unwrap_or_else(|| "UNSUPPORTED".into()))
                .await;
            return;
        }

        // PDF text-layer probe decides native vs scanned.
        let mut route = decision.route;
        if decision.detected_type == "application/pdf" {
            match self.sidecar.pdf_probe(&path.to_string_lossy()) {
                Ok((median, _pages)) => {
                    if median < PDF_TEXT_MEDIAN_CHARS {
                        route = Route::Scanned;
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("password") || msg.contains("encrypted") {
                        // Retrying can't fix a password. No retry.
                        self.flag(&sha, &path, "ENCRYPTED:password protected".into()).await;
                        return;
                    }
                    // transient? one implicit retry happens via convert below
                }
            }
        }
        let _ = self.ledger.update_fields(&sha, &[("route", Some(format!("{route:?}").to_lowercase()))]);

        // ---- Convert (retry ladder row 1-2) --------------------------------
        let conv = {
            let _permit = self.convert_slots.acquire().await.unwrap();
            self.convert_with_retries(&sha, &path, route).await
        };
        let conv = match conv {
            Some(c) => c,
            None => return, // already flagged inside
        };
        if conv.encrypted {
            self.flag(&sha, &path, "ENCRYPTED:password protected".into()).await;
            return;
        }
        if conv.markdown.trim().len() < 30 {
            self.flag(&sha, &path, "CONVERT_FAIL:empty extraction".into()).await;
            return;
        }
        // Multi-doc packet heuristic: several letterhead/date resets.
        let mut extra_soft: Vec<String> = Vec::new();
        if conv.letterhead_resets >= 2 {
            extra_soft.push("POSSIBLE_MULTIDOC".into());
        }
        let _ = self.ledger.set_state(&sha, JobState::Converted);
        self.emit_update(&sha);

        // Cache markdown for the review pane and Ettin training.
        let cache = self.cfg.cache_dir.join(format!("{sha}.md"));
        let _ = std::fs::create_dir_all(&self.cfg.cache_dir);
        let _ = std::fs::write(&cache, &conv.markdown);

        // ---- Filter --------------------------------------------------------
        let ettin_enabled = !self.cfg.ettin_model_dir.is_empty();
        let filtered = match filter::build_evidence(
            &self.sidecar,
            &conv.markdown,
            conv.doc_meta_dates.clone(),
            ettin_enabled,
            self.cfg.evidence_token_budget,
        ) {
            Ok(f) => f,
            Err(e) => {
                self.flag(&sha, &path, format!("RUNTIME_FAIL:filter {e}")).await;
                return;
            }
        };
        let ev = filtered.evidence;
        let _ = self.ledger.update_fields(
            &sha,
            &[
                ("doc_type", Some(ev.doc_type.clone())),
                ("language", Some(ev.language.clone())),
            ],
        );
        let _ = self.ledger.set_state(&sha, JobState::Filtered);
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

        let validated = {
            let _permit = self.slm_slots.acquire().await.unwrap();
            self.name_with_retries(&sha, &ev, &checker, &meta_dates, &modified_iso, ettin_date.as_deref())
                .await
        };
        let mut validated = match validated {
            Some(v) => v,
            None => {
                self.flag(&sha, &path, "SLM_FAIL:no valid output after escalation".into()).await;
                return;
            }
        };
        validated.soft_flags.extend(extra_soft);
        let _ = self.ledger.set_state(&sha, JobState::Validated);

        // ---- Compose final name, dedupe, emit ------------------------------
        let base = match self.ledger.dedupe_name(&validated.base_name, &ext, &sha) {
            Ok(b) => b,
            Err(e) => {
                self.flag(&sha, &path, format!("RUNTIME_FAIL:dedupe {e}")).await;
                return;
            }
        };
        let final_filename = format!("{base}.{ext}");
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

        self.pacer.permit().await;
        let m = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            sha256: sha.clone(),
            status: "ok".into(),
            original_name: name,
            original_relpath: relpath(&self.cfg.processing_dir, &path),
            new_filename: Some(final_filename),
            description: Some(validated.description),
            date: Some(validated.date_iso),
            date_source: Some(validated.date_source),
            doc_type: Some(ev.doc_type),
            language: Some(ev.language),
            duplicate_of: None,
            soft_flags: validated.soft_flags,
            flag_reason: None,
            model_versions: self.model_versions.clone(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        match write_manifest(&self.cfg.manifests_dir(), &m) {
            Ok(_) => {
                let _ = self.ledger.set_state(&sha, JobState::Emitted);
                let _ = self.ledger.log_event(&sha, "emit", "manifest written");
            }
            Err(e) => {
                self.flag(&sha, &path, format!("RUNTIME_FAIL:manifest {e}")).await;
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
    ) -> Option<ConvertResult> {
        let p = path.to_string_lossy().to_string();
        let (hp, tp) = (self.cfg.max_head_pages, self.cfg.max_tail_pages);
        for attempt in 1..=self.cfg.max_stage_attempts {
            let result = match (route, attempt) {
                // Native path: MarkItDown; attempt 3 falls through to raw
                // pdfium text dump / OCR inside the sidecar's fallback op.
                (Route::Native, 1 | 2) => self.sidecar.convert(&p, hp, tp),
                (Route::Native, _) => self.sidecar.ocr(&p, 300, hp, tp),
                // Scanned path: 300 DPI, then 400 DPI, then VL-Extract
                // (the sidecar switches engine on the vl flag via dpi=0).
                (Route::Scanned, 1) => self.sidecar.ocr(&p, 300, hp, tp),
                (Route::Scanned, 2) => self.sidecar.ocr(&p, 400, hp, tp),
                (Route::Scanned, _) => self.sidecar.ocr(&p, 0, hp, tp), // 0 = VL fallback
                (Route::Flag, _) => unreachable!(),
            };
            match result {
                Ok(c) => {
                    if c.ocr_used && c.ocr_mean_conf < OCR_CONF_FLOOR && attempt < self.cfg.max_stage_attempts {
                        let _ = self.ledger.log_event(
                            sha,
                            "convert",
                            &format!("attempt {attempt}: ocr conf {:.2} below floor, escalating", c.ocr_mean_conf),
                        );
                        continue;
                    }
                    return Some(c);
                }
                Err(e) => {
                    let _ = self
                        .ledger
                        .log_event(sha, "convert", &format!("attempt {attempt} failed: {e}"));
                }
            }
        }
        let reason = if route == Route::Scanned { "UNREADABLE" } else { "CONVERT_FAIL" };
        self.flag(sha, path, format!("{reason}:all conversion attempts exhausted")).await;
        None
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
        for attempt in 1..=self.cfg.max_stage_attempts {
            let (tier, bundle) = match attempt {
                1 => (Tier::Primary, ev.bundle.clone()),
                // Attempt 2: primary again, evidence trimmed to 5a-only. If a
                // validator rejected attempt 1, quote the exact violation.
                2 => (Tier::Primary, filter::trimmed_bundle(ev, self.cfg.evidence_token_budget)),
                // Attempt 3: escalate to the 1.2B, evidence budget x2.
                _ => {
                    let mut b = ev.bundle.clone();
                    let cap = self.cfg.evidence_token_budget * 8;
                    if b.len() > cap {
                        b.truncate(cap);
                    }
                    (Tier::Escalation, b)
                }
            };
            let out = self
                .slm
                .name_document(tier, &bundle, &ev.doc_type, &ev.language, violation.as_deref())
                .await;
            match out {
                Ok(o) => match checker.check(&o, &ev.harvest, meta_dates, modified_iso, ettin_date) {
                    Ok(mut v) => {
                        // Ettin/SLM hard disagreement path: one re-prompt with
                        // spans pinned; after that it stays a soft flag.
                        if v.soft_flags.iter().any(|f| f.starts_with("SPAN_MISMATCH"))
                            && attempt == 1
                            && ettin_date.is_some()
                        {
                            violation = Some(format!(
                                "your date disagrees with a high-confidence extracted DATE span ({}); re-examine the evidence spans",
                                ettin_date.unwrap()
                            ));
                            let _ = self.ledger.log_event(sha, "name", "span mismatch, re-prompting");
                            // keep v as a fallback if the retry also mismatches
                            let retry = self
                                .slm
                                .name_document(tier, &bundle, &ev.doc_type, &ev.language, violation.as_deref())
                                .await;
                            if let Ok(o2) = retry {
                                if let Ok(v2) =
                                    checker.check(&o2, &ev.harvest, meta_dates, modified_iso, ettin_date)
                                {
                                    if !v2.soft_flags.iter().any(|f| f.starts_with("SPAN_MISMATCH")) {
                                        return Some(v2);
                                    }
                                }
                            }
                            v.soft_flags.push("SPAN_MISMATCH_PERSISTED".into());
                        }
                        let _ = self.ledger.set_state(sha, JobState::Named);
                        return Some(v);
                    }
                    Err(ce) => {
                        violation = Some(ce.to_string());
                        let _ = self
                            .ledger
                            .log_event(sha, "validate", &format!("attempt {attempt} rejected: {ce}"));
                        if matches!(ce, CheckError::TooLong(_, _)) && attempt >= 2 {
                            // Length problems rarely improve with escalation;
                            // ask for a shorter subject explicitly.
                            violation = Some("subject too long; use at most 6 short words".into());
                        }
                    }
                },
                Err(e) => {
                    violation = None; // model failure, not a validation failure
                    let _ = self.ledger.log_event(sha, "name", &format!("attempt {attempt} SLM error: {e}"));
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
    ) {
        let _ = self.ledger.log_event(sha, "ingest", &format!("duplicate content at {}", path.display()));
        if existing.state != JobState::Emitted {
            return; // duplicate of a flagged file: nothing sane to emit
        }
        let Some(orig_final) = existing.final_filename.clone() else { return };

        // Identity for this *physical* copy: deterministic so replaying the
        // same file is idempotent at Flow 2's SHA gate, and filesystem-safe
        // (NTFS rejects ':' — the old `{sha}:{uuid}` id silently failed to
        // write on Windows and minted a fresh key every run, double-indexing).
        let rel = relpath(&self.cfg.processing_dir, path);
        let dup_key = duplicate_key(sha, &rel);

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
                let stem = orig_final.rsplit_once('.').map(|(s, _)| s).unwrap_or(&orig_final);
                let base = match self.ledger.dedupe_name(stem, ext, &dup_key) {
                    Ok(b) => b,
                    Err(e) => {
                        log::error!("duplicate dedupe failed for {sha}: {e}");
                        return;
                    }
                };
                let fname = format!("{base}.{ext}");
                // Persist the copy as a terminal row so later copies increment
                // past it (otherwise every distinct copy resolves to " (2)"
                // and collides in Flow 2's Archive copy).
                if let Err(e) = self.record_duplicate(&dup_key, path, name, ext, &fname, &orig_final) {
                    log::error!("duplicate ledger record failed for {sha}: {e}");
                    return;
                }
                fname
            }
        };

        self.pacer.permit().await;
        let m = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            sha256: dup_key.clone(),
            status: "ok".into(),
            original_name: name.into(),
            original_relpath: rel,
            new_filename: Some(final_filename),
            description: existing.description.clone(),
            date: existing.proposed_date.clone(),
            date_source: existing.date_source.clone(),
            doc_type: existing.doc_type.clone(),
            language: existing.language.clone(),
            duplicate_of: Some(orig_final),
            soft_flags: vec!["DUPLICATE_CONTENT".into()],
            flag_reason: None,
            model_versions: self.model_versions.clone(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        match write_manifest(&self.cfg.manifests_dir(), &m) {
            Ok(_) => {
                let _ = self.ledger.log_event(sha, "emit", "duplicate manifest written");
            }
            Err(e) => {
                log::error!("duplicate manifest write failed for {dup_key}: {e}");
                let _ = self
                    .ledger
                    .log_event(sha, "emit", &format!("duplicate manifest write FAILED: {e}"));
            }
        }
    }

    /// Durable, terminal ledger row for a physical duplicate copy so
    /// `dedupe_name` sees its filename and later copies resolve to the next
    /// " (n)". Keyed by the copy's deterministic duplicate id, not the shared
    /// content hash.
    fn record_duplicate(
        &self,
        dup_key: &str,
        path: &Path,
        name: &str,
        ext: &str,
        final_filename: &str,
        orig_final: &str,
    ) -> anyhow::Result<()> {
        self.ledger.ingest(dup_key, &path.to_string_lossy(), name, ext)?;
        self.ledger.update_fields(
            dup_key,
            &[
                ("final_filename", Some(final_filename.to_string())),
                ("duplicate_of", Some(orig_final.to_string())),
                ("soft_flags", Some("DUPLICATE_CONTENT".into())),
            ],
        )?;
        self.ledger.set_state(dup_key, JobState::Emitted)?;
        Ok(())
    }

    async fn flag(&self, sha: &str, path: &Path, reason: String) {
        let _ = self.ledger.update_fields(sha, &[("flag_reason", Some(reason.clone()))]);
        let _ = self.ledger.set_state(sha, JobState::Flagged);
        let _ = self.ledger.log_event(sha, "flag", &reason);

        // Move to local quarantine. Never delete; move-or-copy.
        let _ = std::fs::create_dir_all(&self.cfg.quarantine_dir);
        if let Some(fname) = path.file_name() {
            let dest = self.cfg.quarantine_dir.join(fname);
            if std::fs::rename(path, &dest).is_err() {
                let _ = std::fs::copy(path, &dest);
            }
        }

        self.pacer.permit().await;
        let m = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            sha256: sha.to_string(),
            status: "flagged".into(),
            original_name: path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").into(),
            original_relpath: relpath(&self.cfg.processing_dir, path),
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
        let _ = write_manifest(&self.cfg.manifests_dir(), &m);
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
        let job = self.ledger.get(sha)?.ok_or_else(|| anyhow::anyhow!("unknown job"))?;
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
        let today = chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string();
        let v = checker.check(&out, &h, &[date], &today, None)?;

        let base = self.ledger.dedupe_name(&v.base_name, &job.ext, sha)?;
        let final_filename = format!("{base}.{}", job.ext);
        self.ledger.update_fields(
            sha,
            &[
                ("proposed_date", Some(v.date_iso.clone())),
                ("date_source", Some("human".into())),
                ("proposed_subject", Some(v.subject.clone())),
                ("description", Some(v.description.clone())),
                ("final_filename", Some(final_filename.clone())),
                ("flag_reason", None),
                ("soft_flags", Some("HUMAN_CORRECTED".into())),
            ],
        )?;
        self.ledger.log_event(sha, "resubmit", "human correction accepted")?;

        // Quarantined original moves back into scope for Flow 2's rename.
        let quarantined = self.cfg.quarantine_dir.join(&job.original_name);
        let original_relpath = if quarantined.exists() {
            let back = self.cfg.processing_dir.join(&job.original_name);
            let _ = std::fs::rename(&quarantined, &back);
            relpath(&self.cfg.processing_dir, &back)
        } else {
            job.original_name.clone()
        };

        let m = Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            sha256: sha.to_string(),
            status: "ok".into(),
            original_name: job.original_name,
            original_relpath,
            new_filename: Some(final_filename),
            description: Some(v.description),
            date: Some(v.date_iso),
            date_source: Some("human".into()),
            doc_type: job.doc_type,
            language: job.language,
            duplicate_of: None,
            soft_flags: vec!["HUMAN_CORRECTED".into()],
            flag_reason: None,
            model_versions: self.model_versions.clone(),
            processed_at: chrono::Utc::now().to_rfc3339(),
        };
        write_manifest(&self.cfg.manifests_dir(), &m)?;
        self.ledger.set_state(sha, JobState::Emitted)?;
        self.emit_update(sha);
        Ok(())
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

/// Deterministic, filesystem-safe identity for a physical duplicate copy:
/// the shared content hash plus a short digest of the copy's relative path.
/// Same copy -> same key (idempotent replay); distinct copies -> distinct keys.
fn duplicate_key(content_sha: &str, relpath: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(relpath.as_bytes());
    let rp = hex::encode(h.finalize());
    format!("{content_sha}-{}", &rp[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_key_is_deterministic_and_fs_safe() {
        let sha = "a".repeat(64);
        let k1 = duplicate_key(&sha, "sub/dir/file.pdf");
        let k2 = duplicate_key(&sha, "sub/dir/file.pdf");
        assert_eq!(k1, k2, "same copy must yield the same key (idempotent)");
        // No characters NTFS rejects in a filename.
        assert!(!k1.contains([':', '\\', '/', '*', '?', '"', '<', '>', '|']));
        assert!(k1.starts_with(&sha));
    }

    #[test]
    fn duplicate_key_differs_per_copy() {
        let sha = "b".repeat(64);
        assert_ne!(
            duplicate_key(&sha, "a/one.pdf"),
            duplicate_key(&sha, "a/two.pdf"),
            "distinct copies must get distinct keys so each gets its own row"
        );
    }
}
