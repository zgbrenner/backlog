use crate::identity::{instance_id, normalize_relpath};
use crate::ledger::{InstanceState, Ledger};
use crate::manifest::{
    replace_flagged_manifest, write_manifest, Manifest, MANIFEST_SCHEMA_VERSION,
};
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Barrier};

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn final_component(path: &str) -> &str {
    path.rsplit(|ch| ch == '/' || ch == '\\')
        .next()
        .unwrap_or("document.pdf")
}

fn register(ledger: &Ledger, relpath: &str) -> String {
    let normalized = normalize_relpath(relpath);
    let id = instance_id(SHA, &normalized);
    ledger
        .register_instance(&id, SHA, relpath, final_component(relpath), "pdf")
        .expect("register instance");
    id
}

fn ok_manifest(manifest_id: String, original_relpath: &str, new_filename: &str) -> Manifest {
    Manifest {
        schema: MANIFEST_SCHEMA_VERSION,
        manifest_id,
        sha256: SHA.into(),
        status: "ok".into(),
        original_name: final_component(original_relpath).into(),
        original_relpath: original_relpath.into(),
        new_filename: Some(new_filename.into()),
        description: Some("Synthetic agreement used to test deterministic handoff.".into()),
        date: Some("2026-07-21".into()),
        date_source: Some("document".into()),
        doc_type: Some("agreement".into()),
        language: Some("en".into()),
        duplicate_of: None,
        soft_flags: vec![],
        flag_reason: None,
        model_versions: json!({"primary": "fixture"}),
        processed_at: "2026-07-21T12:00:00Z".into(),
    }
}

fn flagged_manifest(manifest_id: String, original_relpath: &str) -> Manifest {
    Manifest {
        schema: MANIFEST_SCHEMA_VERSION,
        manifest_id,
        sha256: SHA.into(),
        status: "flagged".into(),
        original_name: final_component(original_relpath).into(),
        original_relpath: original_relpath.into(),
        new_filename: None,
        description: None,
        date: None,
        date_source: None,
        doc_type: None,
        language: None,
        duplicate_of: None,
        soft_flags: vec![],
        flag_reason: Some("NEEDS_REVIEW:synthetic fixture".into()),
        model_versions: json!({"primary": "fixture"}),
        processed_at: "2026-07-21T12:00:00Z".into(),
    }
}

#[test]
fn registering_the_same_physical_instance_is_replay_safe() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).unwrap();
    let id = register(&ledger, "Clients/Acme/Agreement.pdf");

    let replay = ledger
        .register_instance(
            &id,
            SHA,
            "Clients/Acme/Agreement.pdf",
            "Agreement.pdf",
            "pdf",
        )
        .unwrap()
        .expect("existing instance returned on replay");

    assert_eq!(replay.instance_id, id);
    assert_eq!(replay.manifest_id, id);
    assert_eq!(replay.state, InstanceState::Discovered);
}

#[test]
fn three_duplicate_content_instances_get_three_stable_filenames() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).unwrap();
    let ids = [
        register(&ledger, "one/Agreement.pdf"),
        register(&ledger, "two/Agreement.pdf"),
        register(&ledger, "three/Agreement.pdf"),
    ];

    let names: Vec<String> = ids
        .iter()
        .map(|id| {
            ledger
                .reserve_filename(id, "2026-07-21 Acme Agreement", "pdf")
                .unwrap()
        })
        .collect();

    assert_eq!(
        names,
        vec![
            "2026-07-21 Acme Agreement.pdf",
            "2026-07-21 Acme Agreement (2).pdf",
            "2026-07-21 Acme Agreement (3).pdf",
        ]
    );
    assert_eq!(
        ledger
            .reserve_filename(&ids[1], "A different proposal on replay", "pdf")
            .unwrap(),
        names[1]
    );
}

#[test]
fn concurrent_reservations_cannot_receive_the_same_filename() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(Ledger::open(&dir.path().join("ledger.db")).unwrap());
    let ids: Vec<String> = (0..8)
        .map(|n| register(&ledger, &format!("batch-{n}/Agreement.pdf")))
        .collect();
    let barrier = Arc::new(Barrier::new(ids.len()));

    let handles: Vec<_> = ids
        .into_iter()
        .map(|id| {
            let ledger = ledger.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                ledger
                    .reserve_filename(&id, "2026-07-21 Acme Agreement", "pdf")
                    .unwrap()
            })
        })
        .collect();

    let names: Vec<String> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
    let unique: HashSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len());
}

#[test]
fn manifest_v2_uses_manifest_identity_without_mutating_content_identity() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_id = instance_id(SHA, &normalize_relpath("one/Agreement.pdf"));
    let manifest = ok_manifest(
        manifest_id.clone(),
        "one/Agreement.pdf",
        "2026-07-21 Acme Agreement.pdf",
    );

    let path = write_manifest(dir.path(), &manifest).unwrap();
    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        format!("{manifest_id}.json")
    );

    let written: Manifest = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(written.schema, 2);
    assert_eq!(written.manifest_id, manifest_id);
    assert_eq!(written.sha256, SHA);
}

#[test]
fn unsafe_manifest_identity_is_rejected_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = ok_manifest(
        "../outside".into(),
        "one/Agreement.pdf",
        "2026-07-21 Acme Agreement.pdf",
    );

    assert!(write_manifest(dir.path(), &manifest).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn human_review_can_replace_only_the_matching_flagged_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_id = instance_id(SHA, &normalize_relpath("one/Agreement.pdf"));
    let flagged = flagged_manifest(manifest_id.clone(), "one/Agreement.pdf");
    write_manifest(dir.path(), &flagged).unwrap();

    let mut corrected = ok_manifest(
        manifest_id.clone(),
        "one/Agreement.pdf",
        "2026-07-21 Corrected Agreement.pdf",
    );
    corrected.processed_at = "2026-07-21T12:05:00Z".into();
    replace_flagged_manifest(dir.path(), &corrected).unwrap();

    let path = dir.path().join(format!("{manifest_id}.json"));
    let written: Manifest = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(written.status, "ok");
    assert_eq!(written.new_filename, corrected.new_filename);

    let mut wrong_content = corrected;
    wrong_content.sha256 = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into();
    assert!(replace_flagged_manifest(dir.path(), &wrong_content).is_err());
}
