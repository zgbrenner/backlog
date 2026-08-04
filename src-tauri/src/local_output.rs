//! Native, receipt-backed output delivery.
//!
//! This module deliberately knows nothing about Power Automate.  Its private
//! intent/staging area sits under the selected Local Output root, while the
//! public receipt is the authority for a completed physical delivery.

use crate::ledger::Job;
use crate::manifest::Manifest;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const RECEIPT_SCHEMA: u32 = 1;
const COPY_BUFFER: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Receipt {
    pub receipt_schema: u32,
    pub delivery_mode: String,
    pub output_relpath: Option<String>,
    #[serde(default)]
    pub source_root: String,
    #[serde(default)]
    pub source_path: String,
    #[serde(flatten)]
    pub manifest: Manifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Intent {
    intent_schema: u32,
    #[serde(default)]
    output_base_name: String,
    /// Once an existing output forced a collision, this intent must never
    /// adopt that path merely because its bytes happen to match. Recovery uses
    /// the marker to reserve and rewrite the deterministic suffix instead.
    #[serde(default)]
    collision_observed: bool,
    #[serde(flatten)]
    receipt: Receipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverResult {
    Delivered,
    /// A file already occupies this operator-owned output name but no matching
    /// receipt proves it belongs to this delivery. The caller must reserve the
    /// deterministic next suffix; it must never overwrite the file.
    NameCollision,
}

fn private_root(root: &Path) -> PathBuf {
    root.join(".backlog")
}
fn receipts(root: &Path) -> PathBuf {
    private_root(root).join("receipts")
}
fn intents(root: &Path) -> PathBuf {
    private_root(root).join("intents")
}
fn staging(root: &Path) -> PathBuf {
    private_root(root).join("staging")
}
fn receipt_path(root: &Path, id: &str) -> PathBuf {
    receipts(root).join(format!("{id}.json"))
}
fn intent_path(root: &Path, id: &str) -> PathBuf {
    intents(root).join(format!("{id}.json"))
}
fn stage_path(root: &Path, id: &str) -> PathBuf {
    staging(root).join(format!("{id}.part"))
}

fn safe_source(root: &Path, source: &Path, expected_sha: &str) -> anyhow::Result<bool> {
    if !crate::watcher::is_safe_path_under_root(root, source) || !source.is_file() {
        return Ok(false);
    }
    Ok(hash_file(source)? == expected_sha)
}

fn hash_file(path: &Path) -> anyhow::Result<String> {
    let mut reader = BufReader::with_capacity(COPY_BUFFER, File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; COPY_BUFFER];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sync_dir(dir: &Path) {
    // Directory sync is supported on Unix and on many Windows filesystems but
    // is not uniformly available. The file itself is always synced; failure to
    // sync a directory must not turn a complete delivery into a false success.
    if let Ok(file) = File::open(dir) {
        let _ = file.sync_all();
    }
}

fn write_new_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    let _ = fs::remove_file(&tmp);
    {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    // hard_link is create-new on every supported target. Both paths are in
    // the same private directory, so this is a no-replace publish even on
    // platforms whose `rename` would replace an existing destination.
    match fs::hard_link(&tmp, path) {
        Ok(()) => {
            fs::remove_file(&tmp)?;
            sync_dir(parent);
            Ok(true)
        }
        Err(_error) if path.exists() => {
            let _ = fs::remove_file(&tmp);
            let existing = fs::read(path)?;
            anyhow::ensure!(
                existing == bytes,
                "conflicting existing local receipt/intent"
            );
            Ok(false)
        }
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(anyhow::Error::from(error).context(
                "this filesystem does not support safe create-new publication required for Local Output",
            ))
        }
    }
}

fn immutable_intent_matches(existing: &Intent, expected: &Intent) -> bool {
    if existing.intent_schema != 1
        || (!existing.output_base_name.is_empty()
            && existing.output_base_name != expected.output_base_name)
        || existing.receipt.source_root != expected.receipt.source_root
        || existing.receipt.source_path != expected.receipt.source_path
        || existing.receipt.receipt_schema != RECEIPT_SCHEMA
        || existing.receipt.delivery_mode != "local"
        || !output_pair_is_consistent(&existing.receipt)
        || !output_pair_is_consistent(&expected.receipt)
    {
        return false;
    }
    let mut left = existing.receipt.clone();
    let mut right = expected.receipt.clone();
    left.output_relpath = None;
    right.output_relpath = None;
    left.manifest.new_filename = None;
    right.manifest.new_filename = None;
    left == right
}

fn output_pair_is_consistent(receipt: &Receipt) -> bool {
    receipt.output_relpath == receipt.manifest.new_filename
}

/// Returns whether an already durable, immutable-matching intent existed.
#[cfg(test)]
fn write_intent(root: &Path, receipt: &Receipt) -> anyhow::Result<bool> {
    let output_base_name = receipt
        .manifest
        .new_filename
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("local intent has no output name"))?;
    write_intent_with_base(root, receipt, output_base_name)
}

fn write_intent_with_base(
    root: &Path,
    receipt: &Receipt,
    output_base_name: &str,
) -> anyhow::Result<bool> {
    write_intent_state(root, receipt, output_base_name, false)
}

fn write_intent_state(
    root: &Path,
    receipt: &Receipt,
    output_base_name: &str,
    collision_observed: bool,
) -> anyhow::Result<bool> {
    anyhow::ensure!(
        Path::new(output_base_name).components().count() == 1,
        "unsafe local intent collision base"
    );
    let intent = Intent {
        intent_schema: 1,
        output_base_name: output_base_name.to_string(),
        collision_observed,
        receipt: receipt.clone(),
    };
    let bytes = serde_json::to_vec(&intent)?;
    let path = intent_path(root, &receipt.manifest.manifest_id);
    if path.exists() {
        let existing: Intent = serde_json::from_slice(&fs::read(&path)?)?;
        anyhow::ensure!(
            immutable_intent_matches(&existing, &intent),
            "conflicting local intent"
        );
        if existing.receipt.output_relpath != receipt.output_relpath
            || existing.output_base_name.is_empty()
            || existing.collision_observed != collision_observed
        {
            let tmp = path.with_extension("json.rewrite.tmp");
            let _ = fs::remove_file(&tmp);
            {
                let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
                file.write_all(&bytes)?;
                file.flush()?;
                file.sync_all()?;
            }
            replace_file(&tmp, &path)?;
        }
        return Ok(true);
    }
    let _ = write_new_atomic(&path, &bytes)?;
    Ok(false)
}

fn mark_collision_intent(root: &Path, manifest_id: &str) -> anyhow::Result<()> {
    let path = intent_path(root, manifest_id);
    let mut intent: Intent = serde_json::from_slice(&fs::read(&path)?)?;
    intent.collision_observed = true;
    rewrite_intent(root, manifest_id, &intent)
}

fn existing_receipt_is_complete(root: &Path, receipt: &Receipt) -> anyhow::Result<bool> {
    let path = receipt_path(root, &receipt.manifest.manifest_id);
    if !path.exists() {
        return Ok(false);
    }
    let existing: Receipt = serde_json::from_slice(&fs::read(path)?)?;
    anyhow::ensure!(existing == *receipt, "conflicting local receipt");
    let Some(relative) = &existing.output_relpath else {
        return Ok(true);
    };
    let output = root.join(relative);
    Ok(output.is_file() && hash_file(&output)? == receipt.manifest.sha256)
}

fn may_replace_flagged_receipt(root: &Path, receipt: &Receipt) -> anyhow::Result<bool> {
    let path = receipt_path(root, &receipt.manifest.manifest_id);
    if !path.exists() {
        return Ok(false);
    }
    let existing: Receipt = serde_json::from_slice(&fs::read(path)?)?;
    Ok(existing.receipt_schema == RECEIPT_SCHEMA
        && existing.delivery_mode == "local"
        && receipt.receipt_schema == RECEIPT_SCHEMA
        && receipt.delivery_mode == "local"
        && existing.output_relpath.is_none()
        && output_pair_is_consistent(&existing)
        && output_pair_is_consistent(receipt)
        && existing.source_root == receipt.source_root
        && existing.source_path == receipt.source_path
        && existing.manifest.schema == receipt.manifest.schema
        && existing.manifest.status == "flagged"
        && existing.manifest.manifest_id == receipt.manifest.manifest_id
        && existing.manifest.sha256 == receipt.manifest.sha256
        && existing.manifest.original_name == receipt.manifest.original_name
        && crate::identity::normalize_relpath(&existing.manifest.original_relpath)
            == crate::identity::normalize_relpath(&receipt.manifest.original_relpath)
        && existing.manifest.duplicate_of == receipt.manifest.duplicate_of)
}

fn replace_flagged_receipt(root: &Path, receipt: &Receipt, bytes: &[u8]) -> anyhow::Result<()> {
    let path = receipt_path(root, &receipt.manifest.manifest_id);
    anyhow::ensure!(
        may_replace_flagged_receipt(root, receipt)?,
        "conflicting local receipt"
    );
    let tmp = path.with_extension("json.corrected.tmp");
    let _ = fs::remove_file(&tmp);
    {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    // This is the one authorized receipt-state transition (Flagged -> ok).
    // If this platform cannot replace atomically, it fails closed and leaves
    // the quarantined source for retry rather than creating a second receipt.
    replace_file(&tmp, &path).context("cannot atomically replace flagged local receipt")?;
    sync_dir(receipts(root).as_path());
    Ok(())
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both pointers reference NUL-terminated UTF-16 buffers that live
    // through the call, and the flag is the documented replacement bit.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

fn copy_to_stage(
    source_root: &Path,
    source: &Path,
    stage: &Path,
    expected_sha: &str,
) -> anyhow::Result<()> {
    if stage.exists() {
        if hash_file(stage)? == expected_sha {
            return Ok(());
        }
        // A delivery-owned partial is the only artifact we may clean before
        // completion. Never repair it from a changed or escaped source.
        anyhow::ensure!(
            safe_source(source_root, source, expected_sha)?,
            "corrupt staging cannot be repaired because the source changed or is unsafe"
        );
        fs::remove_file(stage)?;
    }
    let parent = stage.parent().unwrap();
    fs::create_dir_all(parent)?;
    let input = File::open(source)?;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stage)?;
    let mut reader = BufReader::with_capacity(COPY_BUFFER, input);
    let mut writer = BufWriter::with_capacity(COPY_BUFFER, output);
    std::io::copy(&mut reader, &mut writer)?;
    writer.flush()?;
    writer.into_inner()?.sync_all()?;
    anyhow::ensure!(
        hash_file(stage)? == expected_sha,
        "staging SHA-256 mismatch"
    );
    Ok(())
}

/// Copy, publish and receipt an ordinary or corrected local delivery. No
/// operation here writes to a configured Power Automate Outbox.
pub fn deliver(
    root: &Path,
    source_root: &Path,
    source: &Path,
    manifest: &Manifest,
) -> anyhow::Result<DeliverResult> {
    let collision_base = manifest
        .new_filename
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing output name"))?;
    deliver_with_collision_base(root, source_root, source, collision_base, manifest)
}

pub fn deliver_with_collision_base(
    root: &Path,
    source_root: &Path,
    source: &Path,
    collision_base: &str,
    manifest: &Manifest,
) -> anyhow::Result<DeliverResult> {
    deliver_with_remove(
        root,
        source_root,
        source,
        collision_base,
        manifest,
        |path| fs::remove_file(path),
    )
}

pub(crate) fn deliver_with_remove(
    root: &Path,
    source_root: &Path,
    source: &Path,
    collision_base: &str,
    manifest: &Manifest,
    mut remove_source: impl FnMut(&Path) -> std::io::Result<()>,
) -> anyhow::Result<DeliverResult> {
    manifest.validate()?;
    anyhow::ensure!(
        manifest.status == "ok",
        "local file delivery requires an ok manifest"
    );
    let name = manifest
        .new_filename
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing output name"))?;
    anyhow::ensure!(
        Path::new(name).components().count() == 1,
        "unsafe output name"
    );
    anyhow::ensure!(
        collision_suffix(collision_base, name).is_ok(),
        "output name is outside its collision sequence"
    );
    let receipt = Receipt {
        receipt_schema: RECEIPT_SCHEMA,
        delivery_mode: "local".into(),
        output_relpath: Some(name.into()),
        source_root: source_root.to_string_lossy().into_owned(),
        source_path: source.to_string_lossy().into_owned(),
        manifest: manifest.clone(),
    };
    let replacing_flagged = may_replace_flagged_receipt(root, &receipt)?;
    if !replacing_flagged && existing_receipt_is_complete(root, &receipt)? {
        // A missing source is the expected post-delete state. If it remains,
        // it must still be the exact recorded Processing/quarantine file.
        if source.exists() {
            anyhow::ensure!(
                safe_source(source_root, source, &manifest.sha256)?,
                "source changed or unsafe during replay"
            );
            remove_source(source)?;
        }
        let _ = fs::remove_file(stage_path(root, &manifest.manifest_id));
        let _ = fs::remove_file(intent_path(root, &manifest.manifest_id));
        return Ok(DeliverResult::Delivered);
    }
    anyhow::ensure!(
        safe_source(source_root, source, &manifest.sha256)?,
        "source changed or unsafe before local delivery"
    );
    fs::create_dir_all(receipts(root))?;
    fs::create_dir_all(intents(root))?;
    fs::create_dir_all(staging(root))?;
    let stage = stage_path(root, &manifest.manifest_id);
    let output = root.join(name);
    // An output can only be adopted after a crash-after-publish when the
    // exact intent was already durable. A same-hash file with no intent is
    // still someone else's file and must receive a suffix.
    let expected_intent = Intent {
        intent_schema: 1,
        output_base_name: collision_base.to_string(),
        collision_observed: false,
        receipt: receipt.clone(),
    };
    let intent_authorizes_output = if intent_path(root, &manifest.manifest_id).exists() {
        let existing: Intent =
            serde_json::from_slice(&fs::read(intent_path(root, &manifest.manifest_id))?)?;
        anyhow::ensure!(
            immutable_intent_matches(&existing, &expected_intent),
            "conflicting local intent"
        );
        !existing.collision_observed && existing.receipt.output_relpath == receipt.output_relpath
    } else {
        false
    };
    if output.exists() && !intent_authorizes_output {
        let _ = write_intent_state(root, &receipt, collision_base, true)?;
        return Ok(DeliverResult::NameCollision);
    }
    let _ = write_intent_with_base(root, &receipt, collision_base)?;
    copy_to_stage(source_root, source, &stage, &manifest.sha256)?;
    if !output.exists() {
        match fs::hard_link(&stage, &output) {
            Ok(()) => sync_dir(root),
            Err(_error) if output.exists() => {
                mark_collision_intent(root, &manifest.manifest_id)?;
                return Ok(DeliverResult::NameCollision);
            }
            Err(error) => {
                return Err(anyhow::Error::from(error).context(
                    "safe no-replace local output publication is unsupported by this filesystem",
                ))
            }
        }
    }
    if hash_file(&output)? != manifest.sha256 {
        mark_collision_intent(root, &manifest.manifest_id)?;
        return Ok(DeliverResult::NameCollision);
    }
    let bytes = serde_json::to_vec_pretty(&receipt)?;
    if replacing_flagged {
        replace_flagged_receipt(root, &receipt, &bytes)?;
    } else {
        let _ = write_new_atomic(&receipt_path(root, &manifest.manifest_id), &bytes)?;
    }
    anyhow::ensure!(
        existing_receipt_is_complete(root, &receipt)?,
        "local receipt verification failed"
    );
    anyhow::ensure!(
        safe_source(source_root, source, &manifest.sha256)?,
        "source changed or unsafe before deletion"
    );
    remove_source(source)?;
    let _ = fs::remove_file(stage);
    let _ = fs::remove_file(intent_path(root, &manifest.manifest_id));
    Ok(DeliverResult::Delivered)
}

#[cfg(test)]
pub(crate) fn deliver_with_remove_for_test(
    root: &Path,
    source_root: &Path,
    source: &Path,
    manifest: &Manifest,
    remove_source: impl Fn(&Path) -> std::io::Result<()>,
) -> anyhow::Result<DeliverResult> {
    let collision_base = manifest
        .new_filename
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing output name"))?;
    deliver_with_remove(
        root,
        source_root,
        source,
        collision_base,
        manifest,
        remove_source,
    )
}

/// Durable review decision record. Flagged/dismissed records intentionally
/// retain no output path; their quarantined source remains the review copy.
pub fn record_review(
    root: &Path,
    source_root: &Path,
    source: &Path,
    manifest: &Manifest,
) -> anyhow::Result<()> {
    manifest.validate()?;
    anyhow::ensure!(
        matches!(manifest.status.as_str(), "flagged" | "dismissed"),
        "not a review manifest"
    );
    let receipt = Receipt {
        receipt_schema: RECEIPT_SCHEMA,
        delivery_mode: "local".into(),
        output_relpath: None,
        source_root: source_root.to_string_lossy().into_owned(),
        source_path: source.to_string_lossy().into_owned(),
        manifest: manifest.clone(),
    };
    anyhow::ensure!(
        !intent_path(root, &manifest.manifest_id).exists(),
        "a local delivery intent is pending; recover it before changing review state"
    );
    let bytes = serde_json::to_vec_pretty(&receipt)?;
    let path = receipt_path(root, &manifest.manifest_id);
    if path.exists() {
        let existing: Receipt = serde_json::from_slice(&fs::read(&path)?)?;
        if existing == receipt {
            return Ok(());
        }
        // Human review is the documented state transition for this delivery.
        // Replace atomically only a prior flagged record; no output file is
        // involved and a dismissed record can never be reopened.
        anyhow::ensure!(
            existing.manifest.status == "flagged" && manifest.status == "dismissed",
            "conflicting local review receipt"
        );
        replace_flagged_receipt(root, &receipt, &bytes)?;
        return Ok(());
    }
    let _ = write_new_atomic(&path, &bytes)?;
    Ok(())
}

pub fn receipt_is_complete(root: &Path, manifest: &Manifest) -> anyhow::Result<bool> {
    let Some(receipt) = read_receipt(root, &manifest.manifest_id)? else {
        return Ok(false);
    };
    anyhow::ensure!(
        receipt.receipt_schema == RECEIPT_SCHEMA
            && receipt.delivery_mode == "local"
            && output_pair_is_consistent(&receipt)
            && receipt.manifest == *manifest,
        "conflicting local receipt"
    );
    let Some(relative) = &receipt.output_relpath else {
        return Ok(true);
    };
    let output = root.join(relative);
    Ok(output.is_file() && hash_file(&output)? == manifest.sha256)
}

pub fn read_receipt(root: &Path, manifest_id: &str) -> anyhow::Result<Option<Receipt>> {
    let path = receipt_path(root, manifest_id);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

/// Whether this exact delivery has crossed the durable Local transaction
/// boundary. A matching intent is sufficient even before publication; an ok
/// receipt is sufficient after publication. Callers use this distinction to
/// decide whether a failed live attempt may safely roll its ledger reservation
/// and review lease back or must leave them pinned for startup recovery.
pub fn durable_transaction_exists(
    root: &Path,
    source_root: &Path,
    source: &Path,
    collision_base: &str,
    manifest: &Manifest,
) -> anyhow::Result<bool> {
    let expected = Receipt {
        receipt_schema: RECEIPT_SCHEMA,
        delivery_mode: "local".into(),
        output_relpath: manifest.new_filename.clone(),
        source_root: source_root.to_string_lossy().into_owned(),
        source_path: source.to_string_lossy().into_owned(),
        manifest: manifest.clone(),
    };
    let intent_file = intent_path(root, &manifest.manifest_id);
    if intent_file.is_file() {
        let bytes = fs::read(intent_file)?;
        if let Ok(intent) = serde_json::from_slice::<Intent>(&bytes) {
            let expected_intent = Intent {
                intent_schema: 1,
                output_base_name: collision_base.to_string(),
                collision_observed: false,
                receipt: expected.clone(),
            };
            if immutable_intent_matches(&intent, &expected_intent) {
                return Ok(true);
            }
        }
    }
    let receipt_file = receipt_path(root, &manifest.manifest_id);
    if receipt_file.is_file() {
        let bytes = fs::read(receipt_file)?;
        if let Ok(receipt) = serde_json::from_slice::<Receipt>(&bytes) {
            return Ok(receipt.manifest.status == "ok"
                && immutable_intent_matches(
                    &Intent {
                        intent_schema: 1,
                        output_base_name: collision_base.to_string(),
                        collision_observed: false,
                        receipt,
                    },
                    &Intent {
                        intent_schema: 1,
                        output_base_name: collision_base.to_string(),
                        collision_observed: false,
                        receipt: expected,
                    },
                ));
        }
    }
    Ok(false)
}

/// Validate persisted intent provenance against the ledger-owned contract
/// before allowing recovery to touch a source file.
pub fn validate_intent_for_recovery(
    root: &Path,
    job: &Job,
    expected_original_relpath: &str,
    expected_source_root: &Path,
    expected_source: &Path,
) -> anyhow::Result<bool> {
    let path = intent_path(root, &job.delivery_id);
    if !path.is_file() {
        return Ok(false);
    }
    let intent: Intent = serde_json::from_slice(&fs::read(path)?)?;
    let manifest = &intent.receipt.manifest;
    let expected_duplicate_of =
        (job.sha256 != job.content_sha256).then_some(job.content_sha256.as_str());
    let manifest_name = manifest
        .new_filename
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("persisted intent has no output name"))?;
    let current_name = job
        .final_filename
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("local intent has no pinned output name"))?;
    let persisted_base = persisted_intent_base(&intent, manifest_name)?;
    let name_is_current = manifest_name == current_name;
    let name_is_recorded_previous =
        job.recovery_previous_filename
            .as_deref()
            .is_some_and(|previous| {
                manifest_name == previous
                    && collision_suffix(&persisted_base, previous).is_ok()
                    && collision_suffix(&persisted_base, current_name).is_ok()
            });
    // A human correction's values are intentionally not compared to the old
    // flagged row: the authoritative correction payload exists only in its
    // durable intent until its review lease commits. Every ordinary/duplicate
    // attempt, by contrast, snapshots these values into the ledger before it
    // can create an intent, so replay must never let JSON rewrite them.
    let correction_in_progress = job.state == crate::ledger::JobState::Flagged
        && job.review_operation.as_deref() == Some("correct");
    let metadata_matches = if correction_in_progress {
        true
    } else {
        let expected_model_versions: serde_json::Value = job
            .model_versions
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("local intent has no persisted model provenance"))?;
        let expected_soft_flags = job
            .soft_flags
            .as_deref()
            .filter(|flags| !flags.is_empty())
            .map(|flags| flags.split(',').map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default();
        manifest.date == job.proposed_date
            && manifest.date_source == job.date_source
            && manifest.description == job.description
            && manifest.doc_type == job.doc_type
            && manifest.language == job.language
            && manifest.model_versions == expected_model_versions
            && manifest.soft_flags == expected_soft_flags
    };
    anyhow::ensure!(
        intent.intent_schema == 1
            && intent.receipt.receipt_schema == RECEIPT_SCHEMA
            && intent.receipt.delivery_mode == "local"
            && manifest.status == "ok"
            && manifest.validate().is_ok()
            && manifest.manifest_id == job.delivery_id
            && manifest.sha256 == job.content_sha256
            && manifest.original_name == job.original_name
            && crate::identity::normalize_relpath(&manifest.original_relpath)
                == crate::identity::normalize_relpath(expected_original_relpath)
            && manifest.duplicate_of.as_deref() == expected_duplicate_of
            && (name_is_current || name_is_recorded_previous)
            && intent.receipt.output_relpath == manifest.new_filename
            && collision_suffix(&persisted_base, current_name).is_ok()
            && crate::identity::normalize_relpath(&intent.receipt.source_root)
                == crate::identity::normalize_relpath(&expected_source_root.to_string_lossy())
            && crate::identity::normalize_relpath(&intent.receipt.source_path)
                == crate::identity::normalize_relpath(&expected_source.to_string_lossy())
            && metadata_matches,
        "local intent provenance does not match pinned ledger delivery"
    );
    Ok(true)
}

/// Resume the exact durable transaction recorded before Local publication.
///
/// The intent, rather than today's pipeline settings or a freshly assembled
/// manifest, is authoritative: it pins the source root/path, output name and
/// `processed_at` metadata. This converges every crash point after intent
/// creation and fails closed if any persisted identity or source bytes drift.
#[cfg(test)]
pub fn recover_intent(root: &Path, manifest_id: &str) -> anyhow::Result<Option<Receipt>> {
    recover_intent_inner(root, manifest_id, None, |_, _| Ok(true), || Ok(()))
}

/// Resume a ledger-bound Local intent. The ledger advances its reservation
/// before this function writes a collision suffix, so a restart can accept
/// only the exact current name or the one recorded predecessor and normalize
/// the latter before it touches an output/source pair.
pub fn recover_intent_with_name_sync(
    root: &Path,
    manifest_id: &str,
    current_name: &str,
    advance_name: impl FnMut(&str, &str) -> anyhow::Result<bool>,
) -> anyhow::Result<Option<Receipt>> {
    recover_intent_inner(root, manifest_id, Some(current_name), advance_name, || {
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn recover_intent_with_name_sync_for_test(
    root: &Path,
    manifest_id: &str,
    current_name: &str,
    advance_name: impl FnMut(&str, &str) -> anyhow::Result<bool>,
    after_collision_rewrite: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<Option<Receipt>> {
    recover_intent_inner(
        root,
        manifest_id,
        Some(current_name),
        advance_name,
        after_collision_rewrite,
    )
}

fn recover_intent_inner(
    root: &Path,
    manifest_id: &str,
    current_name: Option<&str>,
    mut advance_name: impl FnMut(&str, &str) -> anyhow::Result<bool>,
    mut after_collision_rewrite: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<Option<Receipt>> {
    let path = intent_path(root, manifest_id);
    if !path.is_file() {
        return Ok(None);
    }
    let mut intent: Intent = serde_json::from_slice(&fs::read(&path)?)?;
    anyhow::ensure!(intent.intent_schema == 1, "unsupported local intent schema");
    anyhow::ensure!(
        intent.receipt.receipt_schema == RECEIPT_SCHEMA
            && intent.receipt.delivery_mode == "local"
            && intent.receipt.manifest.manifest_id == manifest_id
            && intent.receipt.manifest.status == "ok",
        "invalid local intent delivery identity"
    );
    intent.receipt.manifest.validate()?;
    anyhow::ensure!(
        intent.receipt.output_relpath == intent.receipt.manifest.new_filename,
        "local intent output name does not match its manifest"
    );
    let source_root = PathBuf::from(&intent.receipt.source_root);
    let source = PathBuf::from(&intent.receipt.source_path);
    let manifest_name = intent
        .receipt
        .manifest
        .new_filename
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("persisted intent has no output name"))?;
    let base_name = persisted_intent_base(&intent, manifest_name)?;
    let mut manifest = intent.receipt.manifest.clone();
    if let Some(current_name) = current_name {
        anyhow::ensure!(
            collision_suffix(&base_name, current_name).is_ok(),
            "ledger reservation has a different local intent base"
        );
        if manifest.new_filename.as_deref() != Some(current_name) {
            // The process died after moving the ledger reservation but before
            // rewriting the intent. Never retry the stale output name: it was
            // recorded solely to reach this normalization point.
            manifest.new_filename = Some(current_name.to_string());
            intent.receipt.output_relpath = manifest.new_filename.clone();
            intent.receipt.manifest = manifest.clone();
            intent.collision_observed = false;
            rewrite_intent(root, manifest_id, &intent)?;
        }
    }
    let current_manifest_name = manifest
        .new_filename
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("persisted intent has no output name"))?;
    let first_suffix = collision_suffix(&base_name, current_manifest_name)?
        .saturating_add(1)
        .max(2);
    for suffix in first_suffix..=(crate::checker::MAX_NAME_COLLISIONS + 1) {
        match deliver_with_collision_base(root, &source_root, &source, &base_name, &manifest)? {
            DeliverResult::Delivered => return read_receipt(root, manifest_id),
            DeliverResult::NameCollision => {
                let next_name = suffixed_name(&base_name, suffix)?;
                let current = manifest
                    .new_filename
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("persisted intent has no output name"))?;
                if !advance_name(current, &next_name)? {
                    anyhow::bail!("local recovery reservation changed while resolving collision");
                }
                manifest.new_filename = Some(next_name);
                intent.output_base_name = base_name.clone();
                intent.receipt.output_relpath = manifest.new_filename.clone();
                intent.receipt.manifest = manifest.clone();
                intent.collision_observed = false;
                rewrite_intent(root, manifest_id, &intent)?;
                after_collision_rewrite()?;
            }
        }
    }
    anyhow::bail!("persisted local intent collision limit exceeded")
}

fn collision_suffix(base_name: &str, current_name: &str) -> anyhow::Result<u32> {
    if current_name == base_name {
        return Ok(1);
    }
    for suffix in 2..=(crate::checker::MAX_NAME_COLLISIONS + 1) {
        if suffixed_name(base_name, suffix)? == current_name {
            return Ok(suffix);
        }
    }
    anyhow::bail!("local intent output is not in the bounded collision sequence")
}

fn suffixed_name(name: &str, suffix: u32) -> anyhow::Result<String> {
    let path = Path::new(name);
    anyhow::ensure!(path.components().count() == 1, "unsafe output name");
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(name);
    Ok(match path.extension().and_then(|value| value.to_str()) {
        Some(extension) => format!("{stem} ({suffix}).{extension}"),
        None => format!("{stem} ({suffix})"),
    })
}

fn persisted_intent_base(intent: &Intent, manifest_name: &str) -> anyhow::Result<String> {
    let base = if intent.output_base_name.is_empty() {
        // Compatibility for schema-1 intents written before this field was
        // populated. Only those legacy intents use suffix inference; every
        // new transaction persists the caller's exact original base.
        legacy_base_output_name(manifest_name)?
    } else {
        intent.output_base_name.clone()
    };
    anyhow::ensure!(
        collision_suffix(&base, manifest_name).is_ok(),
        "persisted intent output is outside its collision sequence"
    );
    Ok(base)
}

fn legacy_base_output_name(name: &str) -> anyhow::Result<String> {
    let path = Path::new(name);
    anyhow::ensure!(path.components().count() == 1, "unsafe output name");
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(name);
    let base = stem
        .rsplit_once(" (")
        .filter(|(_, tail)| tail.ends_with(')') && tail[..tail.len() - 1].parse::<u32>().is_ok())
        .map(|(head, _)| head)
        .unwrap_or(stem);
    Ok(match path.extension().and_then(|value| value.to_str()) {
        Some(extension) => format!("{base}.{extension}"),
        None => base.to_string(),
    })
}

fn rewrite_intent(root: &Path, manifest_id: &str, intent: &Intent) -> anyhow::Result<()> {
    let path = intent_path(root, manifest_id);
    let tmp = path.with_extension("json.rewrite.tmp");
    let _ = fs::remove_file(&tmp);
    let bytes = serde_json::to_vec(intent)?;
    {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    replace_file(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(id: &str, sha: &str, name: &str) -> Manifest {
        Manifest {
            schema: crate::manifest::MANIFEST_SCHEMA_VERSION,
            manifest_id: id.into(),
            sha256: sha.into(),
            status: "ok".into(),
            original_name: "scan.pdf".into(),
            original_relpath: "in/scan.pdf".into(),
            new_filename: Some(name.into()),
            description: Some("A safely named test document.".into()),
            date: Some("2026-08-03".into()),
            date_source: Some("document".into()),
            doc_type: Some("letter".into()),
            language: Some("en".into()),
            duplicate_of: None,
            soft_flags: vec![],
            flag_reason: None,
            model_versions: json!({"test": "1"}),
            processed_at: "2026-08-03T00:00:00Z".into(),
        }
    }

    #[test]
    fn ordinary_delivery_is_receipted_and_replay_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        let source = processing.join("scan.pdf");
        fs::write(&source, b"same bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let m = manifest(&id, &sha, "2026-08-03 Test.pdf");

        assert_eq!(
            deliver(&output, &processing, &source, &m).unwrap(),
            DeliverResult::Delivered
        );
        assert!(!source.exists());
        assert_eq!(
            fs::read(output.join("2026-08-03 Test.pdf")).unwrap(),
            b"same bytes"
        );
        assert!(read_receipt(&output, &id).unwrap().is_some());
        assert_eq!(
            deliver(&output, &processing, &source, &m).unwrap(),
            DeliverResult::Delivered
        );
    }

    #[test]
    fn unrelated_output_collision_is_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        fs::create_dir_all(&output).unwrap();
        let source = processing.join("scan.pdf");
        fs::write(&source, b"new bytes").unwrap();
        fs::write(output.join("2026-08-03 Test.pdf"), b"unrelated").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let m = manifest(&id, &sha, "2026-08-03 Test.pdf");

        assert_eq!(
            deliver(&output, &processing, &source, &m).unwrap(),
            DeliverResult::NameCollision
        );
        assert_eq!(
            fs::read(output.join("2026-08-03 Test.pdf")).unwrap(),
            b"unrelated"
        );
        assert!(source.exists());
        assert!(read_receipt(&output, &id).unwrap().is_none());
    }

    #[test]
    fn legitimate_numeric_parenthetical_is_preserved_when_collision_suffix_is_added() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        fs::create_dir_all(&output).unwrap();
        let source = processing.join("form.pdf");
        fs::write(&source, b"owned form bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/form.pdf");
        let base = "2026-08-03 Form (2024).pdf";
        let mut m = manifest(&id, &sha, base);
        fs::write(output.join(base), b"unrelated form").unwrap();

        assert_eq!(
            deliver_with_collision_base(&output, &processing, &source, base, &m).unwrap(),
            DeliverResult::NameCollision
        );
        m.new_filename = Some("2026-08-03 Form (2024) (2).pdf".into());
        assert_eq!(
            deliver_with_collision_base(&output, &processing, &source, base, &m).unwrap(),
            DeliverResult::Delivered
        );
        assert_eq!(fs::read(output.join(base)).unwrap(), b"unrelated form");
        assert_eq!(
            fs::read(output.join("2026-08-03 Form (2024) (2).pdf")).unwrap(),
            b"owned form bytes"
        );
        assert!(!output.join("2026-08-03 Form (2).pdf").exists());
    }

    #[test]
    fn flagged_receipt_can_be_corrected_into_one_local_delivery() {
        let temp = tempfile::tempdir().unwrap();
        let quarantine = temp.path().join("quarantine");
        let output = temp.path().join("output");
        fs::create_dir_all(&quarantine).unwrap();
        let source = quarantine.join("scan.pdf");
        fs::write(&source, b"reviewed bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let mut flagged = manifest(&id, &sha, "2026-08-03 Reviewed.pdf");
        flagged.status = "flagged".into();
        flagged.new_filename = None;
        flagged.description = None;
        flagged.date = None;
        flagged.date_source = None;
        flagged.flag_reason = Some("NEEDS_REVIEW:test".into());
        record_review(&output, &quarantine, &source, &flagged).unwrap();

        let corrected = manifest(&id, &sha, "2026-08-03 Reviewed.pdf");
        assert_eq!(
            deliver(&output, &quarantine, &source, &corrected).unwrap(),
            DeliverResult::Delivered
        );
        assert_eq!(
            read_receipt(&output, &id).unwrap().unwrap().manifest.status,
            "ok"
        );
        assert!(!source.exists());
    }

    #[test]
    fn correction_rejects_flagged_receipt_with_tampered_source_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let quarantine = temp.path().join("quarantine");
        let output = temp.path().join("output");
        fs::create_dir_all(&quarantine).unwrap();
        let source = quarantine.join("scan.pdf");
        fs::write(&source, b"reviewed bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let mut flagged = manifest(&id, &sha, "unused.pdf");
        flagged.status = "flagged".into();
        flagged.new_filename = None;
        flagged.description = None;
        flagged.date = None;
        flagged.date_source = None;
        flagged.flag_reason = Some("NEEDS_REVIEW:test".into());
        record_review(&output, &quarantine, &source, &flagged).unwrap();
        let path = receipt_path(&output, &id);
        let mut tampered: Receipt = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        tampered.source_path = quarantine.join("other.pdf").to_string_lossy().into_owned();
        fs::write(&path, serde_json::to_vec_pretty(&tampered).unwrap()).unwrap();

        let corrected = manifest(&id, &sha, "2026-08-03 Reviewed.pdf");
        assert!(deliver(&output, &quarantine, &source, &corrected).is_err());
        assert!(source.exists());
        assert!(!output.join("2026-08-03 Reviewed.pdf").exists());
    }

    #[test]
    fn same_hash_output_without_intent_is_an_unrelated_collision() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        fs::create_dir_all(&output).unwrap();
        let source = processing.join("scan.pdf");
        fs::write(&source, b"identical bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let m = manifest(&id, &sha, "2026-08-03 Test.pdf");
        fs::write(output.join("2026-08-03 Test.pdf"), b"identical bytes").unwrap();

        assert_eq!(
            deliver(&output, &processing, &source, &m).unwrap(),
            DeliverResult::NameCollision
        );
        assert!(source.exists());
        assert!(read_receipt(&output, &id).unwrap().is_none());
        let intent: Intent =
            serde_json::from_slice(&fs::read(intent_path(&output, &id)).unwrap()).unwrap();
        assert!(
            intent.collision_observed,
            "foreign output is durably marked for suffix recovery"
        );
        let recovered = recover_intent(&output, &id).unwrap().unwrap();
        assert_eq!(
            recovered.output_relpath.as_deref(),
            Some("2026-08-03 Test (2).pdf")
        );
        assert_eq!(
            fs::read(output.join("2026-08-03 Test.pdf")).unwrap(),
            b"identical bytes",
            "same-hash foreign output remains untouched"
        );
        assert_eq!(
            fs::read(output.join("2026-08-03 Test (2).pdf")).unwrap(),
            b"identical bytes"
        );
        assert!(!source.exists());
    }

    #[test]
    fn intent_source_drift_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        let first = processing.join("first.pdf");
        let second = processing.join("second.pdf");
        fs::write(&first, b"same").unwrap();
        fs::write(&second, b"same").unwrap();
        let sha = hash_file(&first).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let m = manifest(&id, &sha, "2026-08-03 Test.pdf");
        let receipt = Receipt {
            receipt_schema: RECEIPT_SCHEMA,
            delivery_mode: "local".into(),
            output_relpath: m.new_filename.clone(),
            source_root: processing.to_string_lossy().into_owned(),
            source_path: first.to_string_lossy().into_owned(),
            manifest: m.clone(),
        };
        fs::create_dir_all(intents(&output)).unwrap();
        write_intent(&output, &receipt).unwrap();

        assert!(deliver(&output, &processing, &second, &m).is_err());
        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    fn corrupt_stage_is_repaired_only_from_a_safe_matching_source() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        fs::create_dir_all(staging(&output)).unwrap();
        let source = processing.join("scan.pdf");
        fs::write(&source, b"good source").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let m = manifest(&id, &sha, "2026-08-03 Test.pdf");
        fs::write(stage_path(&output, &id), b"corrupt partial").unwrap();
        assert_eq!(
            deliver(&output, &processing, &source, &m).unwrap(),
            DeliverResult::Delivered
        );
        assert!(!stage_path(&output, &id).exists());

        let source = processing.join("changed.pdf");
        fs::write(&source, b"good source").unwrap();
        let id = crate::identity::instance_id(&sha, "in/changed.pdf");
        let mut m = manifest(&id, &sha, "2026-08-03 Changed.pdf");
        m.original_relpath = "in/changed.pdf".into();
        fs::create_dir_all(staging(&output)).unwrap();
        fs::write(stage_path(&output, &id), b"corrupt partial").unwrap();
        fs::write(&source, b"changed after plan").unwrap();
        assert!(deliver(&output, &processing, &source, &m).is_err());
        assert!(stage_path(&output, &id).exists());
    }

    #[test]
    fn persisted_intent_recovers_stage_output_and_receipt_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        let source = processing.join("scan.pdf");
        fs::write(&source, b"interrupted").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let m = manifest(&id, &sha, "2026-08-03 Interrupted.pdf");
        let receipt = Receipt {
            receipt_schema: RECEIPT_SCHEMA,
            delivery_mode: "local".into(),
            output_relpath: m.new_filename.clone(),
            source_root: processing.to_string_lossy().into_owned(),
            source_path: source.to_string_lossy().into_owned(),
            manifest: m.clone(),
        };

        // Intent only, then valid staging, then crash-after-output all resume
        // through the same transaction and produce one receipt.
        fs::create_dir_all(intents(&output)).unwrap();
        write_intent(&output, &receipt).unwrap();
        copy_to_stage(&processing, &source, &stage_path(&output, &id), &sha).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::hard_link(
            stage_path(&output, &id),
            output.join("2026-08-03 Interrupted.pdf"),
        )
        .unwrap();
        assert!(recover_intent(&output, &id).unwrap().is_some());
        assert!(!source.exists());
        assert!(receipt_is_complete(&output, &m).unwrap());

        // Receipt + output with source absent is a completed replay. A missing
        // output with the receipt is never accepted as a terminal delivery.
        assert!(recover_intent(&output, &id).unwrap().is_none());
        fs::remove_file(output.join("2026-08-03 Interrupted.pdf")).unwrap();
        assert!(deliver(&output, &processing, &source, &m).is_err());
    }

    #[test]
    fn pending_correction_intent_blocks_dismissal_until_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let quarantine = temp.path().join("quarantine");
        let output = temp.path().join("output");
        fs::create_dir_all(&quarantine).unwrap();
        let source = quarantine.join("scan.pdf");
        fs::write(&source, b"reviewed bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let mut flagged = manifest(&id, &sha, "unused.pdf");
        flagged.status = "flagged".into();
        flagged.new_filename = None;
        flagged.description = None;
        flagged.date = None;
        flagged.date_source = None;
        flagged.flag_reason = Some("NEEDS_REVIEW:test".into());
        record_review(&output, &quarantine, &source, &flagged).unwrap();
        let corrected = manifest(&id, &sha, "2026-08-03 Reviewed.pdf");
        let receipt = Receipt {
            receipt_schema: RECEIPT_SCHEMA,
            delivery_mode: "local".into(),
            output_relpath: corrected.new_filename.clone(),
            source_root: quarantine.to_string_lossy().into_owned(),
            source_path: source.to_string_lossy().into_owned(),
            manifest: corrected.clone(),
        };
        write_intent(&output, &receipt).unwrap();
        let mut dismissed = flagged.clone();
        dismissed.status = "dismissed".into();
        dismissed.flag_reason = Some("DISMISSED:test".into());
        assert!(record_review(&output, &quarantine, &source, &dismissed).is_err());
        assert!(recover_intent(&output, &id).unwrap().is_some());
        assert_eq!(
            read_receipt(&output, &id).unwrap().unwrap().manifest.status,
            "ok"
        );
    }

    #[test]
    fn dismissal_replaces_flagged_receipt_once_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let quarantine = temp.path().join("quarantine");
        fs::create_dir_all(&quarantine).unwrap();
        let source = quarantine.join("scan.pdf");
        fs::write(&source, b"reviewed bytes").unwrap();
        let sha = "a".repeat(64);
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let mut flagged = manifest(&id, &sha, "unused.pdf");
        flagged.status = "flagged".into();
        flagged.new_filename = None;
        flagged.description = None;
        flagged.date = None;
        flagged.date_source = None;
        flagged.flag_reason = Some("NEEDS_REVIEW:test".into());
        record_review(&output, &quarantine, &source, &flagged).unwrap();
        let mut dismissed = flagged.clone();
        dismissed.status = "dismissed".into();
        dismissed.flag_reason = Some("DISMISSED:test".into());
        record_review(&output, &quarantine, &source, &dismissed).unwrap();
        record_review(&output, &quarantine, &source, &dismissed).unwrap();
        assert_eq!(
            read_receipt(&output, &id).unwrap().unwrap().manifest.status,
            "dismissed"
        );
    }

    #[test]
    fn source_delete_failure_keeps_a_recoverable_output_and_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        let source = processing.join("scan.pdf");
        fs::write(&source, b"delete later").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let m = manifest(&id, &sha, "2026-08-03 Delete Later.pdf");

        assert!(deliver_with_remove(
            &output,
            &processing,
            &source,
            "2026-08-03 Delete Later.pdf",
            &m,
            |_| { Err(std::io::Error::other("injected delete failure")) }
        )
        .is_err());
        assert!(source.exists(), "failed deletion retains source");
        assert!(receipt_is_complete(&output, &m).unwrap());
        assert_eq!(
            deliver(&output, &processing, &source, &m).unwrap(),
            DeliverResult::Delivered
        );
        assert!(!source.exists());
        assert_eq!(
            fs::read_dir(receipts(&output)).unwrap().count(),
            1,
            "retry keeps one receipt"
        );
        assert!(!intent_path(&output, &id).exists());
        assert!(!stage_path(&output, &id).exists());
    }

    #[test]
    fn durable_intent_collision_rewrites_consistent_name_pair_and_converges() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        fs::create_dir_all(&output).unwrap();
        let source = processing.join("scan.pdf");
        fs::write(&source, b"owned delivery bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let manifest = manifest(&id, &sha, "2026-08-03 Test.pdf");
        let receipt = Receipt {
            receipt_schema: RECEIPT_SCHEMA,
            delivery_mode: "local".into(),
            output_relpath: manifest.new_filename.clone(),
            source_root: processing.to_string_lossy().into_owned(),
            source_path: source.to_string_lossy().into_owned(),
            manifest: manifest.clone(),
        };
        fs::create_dir_all(intents(&output)).unwrap();
        write_intent(&output, &receipt).unwrap();
        fs::write(output.join("2026-08-03 Test.pdf"), b"foreign bytes").unwrap();

        let recovered = recover_intent(&output, &id).unwrap().unwrap();
        assert_eq!(
            recovered.output_relpath, recovered.manifest.new_filename,
            "receipt output and manifest name are one mutable pair"
        );
        assert_eq!(
            recovered.output_relpath.as_deref(),
            Some("2026-08-03 Test (2).pdf")
        );
        assert_eq!(
            fs::read(output.join("2026-08-03 Test.pdf")).unwrap(),
            b"foreign bytes"
        );
        assert_eq!(
            fs::read(output.join("2026-08-03 Test (2).pdf")).unwrap(),
            b"owned delivery bytes"
        );
        assert!(!source.exists());
        assert!(!intent_path(&output, &id).exists());
        assert!(recover_intent(&output, &id).unwrap().is_none());
        assert_eq!(fs::read_dir(receipts(&output)).unwrap().count(), 1);
    }

    #[test]
    fn durable_recovery_preserves_legitimate_numeric_parenthetical_base() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        fs::create_dir_all(&output).unwrap();
        let source = processing.join("form.pdf");
        fs::write(&source, b"recoverable form bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/form.pdf");
        let base = "2026-08-03 Form (2024).pdf";
        let m = manifest(&id, &sha, base);
        let receipt = Receipt {
            receipt_schema: RECEIPT_SCHEMA,
            delivery_mode: "local".into(),
            output_relpath: m.new_filename.clone(),
            source_root: processing.to_string_lossy().into_owned(),
            source_path: source.to_string_lossy().into_owned(),
            manifest: m,
        };
        fs::create_dir_all(intents(&output)).unwrap();
        write_intent_with_base(&output, &receipt, base).unwrap();
        fs::write(output.join(base), b"unrelated form").unwrap();

        let recovered = recover_intent(&output, &id).unwrap().unwrap();
        assert_eq!(
            recovered.output_relpath.as_deref(),
            Some("2026-08-03 Form (2024) (2).pdf")
        );
        assert_eq!(fs::read(output.join(base)).unwrap(), b"unrelated form");
        assert_eq!(
            fs::read(output.join("2026-08-03 Form (2024) (2).pdf")).unwrap(),
            b"recoverable form bytes"
        );
        assert!(!output.join("2026-08-03 Form (2).pdf").exists());
        assert!(!source.exists());
    }

    #[test]
    fn persisted_intent_uses_a_stable_base_across_multiple_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let processing = temp.path().join("processing");
        let output = temp.path().join("output");
        fs::create_dir_all(&processing).unwrap();
        fs::create_dir_all(&output).unwrap();
        let source = processing.join("scan.pdf");
        fs::write(&source, b"owned delivery bytes").unwrap();
        let sha = hash_file(&source).unwrap();
        let id = crate::identity::instance_id(&sha, "in/scan.pdf");
        let manifest = manifest(&id, &sha, "2026-08-03 Test.pdf");
        let receipt = Receipt {
            receipt_schema: RECEIPT_SCHEMA,
            delivery_mode: "local".into(),
            output_relpath: manifest.new_filename.clone(),
            source_root: processing.to_string_lossy().into_owned(),
            source_path: source.to_string_lossy().into_owned(),
            manifest,
        };
        write_intent(&output, &receipt).unwrap();
        fs::write(output.join("2026-08-03 Test.pdf"), b"foreign one").unwrap();
        fs::write(output.join("2026-08-03 Test (2).pdf"), b"foreign two").unwrap();
        let recovered = recover_intent(&output, &id).unwrap().unwrap();
        assert_eq!(
            recovered.output_relpath.as_deref(),
            Some("2026-08-03 Test (3).pdf")
        );
        assert!(output.join("2026-08-03 Test (3).pdf").is_file());
    }
}
