//! Stable identifiers for physical file instances and emitted manifests.
//!
//! A content SHA-256 identifies bytes. An instance ID identifies those bytes at
//! one durable delivery path, so identical content can be processed separately
//! without overloading the content hash or introducing replay-unsafe randomness.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DELIVERY_DIR_PREFIX: &str = "__bl_";
pub const INCOMING_FILE_PREFIX: &str = "__incoming_";
const DELIVERY_ID_HEX_LEN: usize = 32;
const DELIVERY_MOVE_ATTEMPTS: usize = 5;

/// Normalize a path for instance identity.
///
/// BackLog targets Windows and SharePoint, where path separators and casing are
/// not meaningful distinctions. The normalized value is only an identity input;
/// the original path is still preserved verbatim in the ledger and manifest.
pub fn normalize_relpath(path: &str) -> String {
    let separators_normalized = path.replace('\\', "/");
    let mut parts: Vec<String> = Vec::new();

    for part in separators_normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part.to_lowercase()),
        }
    }

    parts.join("/")
}

/// Derive a stable physical-file identifier from true content identity and its
/// durable delivery path.
pub fn instance_id(content_sha256: &str, normalized_relpath: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content_sha256.trim().to_ascii_lowercase().as_bytes());
    hasher.update([0]);
    hasher.update(normalized_relpath.as_bytes());
    hex::encode(hasher.finalize())
}

/// Instance and manifest identifiers are deliberately filename-safe.
pub fn is_safe_identifier(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Return true when a path already lives beneath a BackLog delivery directory.
pub fn has_delivery_identity(path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .filter_map(|ancestor| ancestor.file_name())
        .filter_map(|name| name.to_str())
        .any(is_delivery_dir_name)
}

/// Move an arrival into a durable sibling delivery directory while preserving
/// its human filename. Power Automate arrivals carry a stable source token in
/// the temporary filename. Manual drops receive a random delivery token once.
/// The move happens before hashing and ledger registration. A crash after the
/// move is safe because the delivery directory persists and the startup sweep
/// discovers it again.
pub fn ensure_delivery_path(path: &Path) -> anyhow::Result<PathBuf> {
    if has_delivery_identity(path) {
        return Ok(path.to_path_buf());
    }

    anyhow::ensure!(path.is_file(), "delivery source is not a file: {}", path.display());
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("delivery source has no parent: {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("delivery source has no UTF-8 filename: {}", path.display()))?;

    if let Some((source_token, original_name)) = parse_incoming_file_name(file_name) {
        let delivery_id = delivery_id_from_source_token(&source_token);
        return move_to_delivery(path, parent, &delivery_id, &original_name, true);
    }

    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 0..DELIVERY_MOVE_ATTEMPTS {
        let delivery_id = uuid::Uuid::new_v4().simple().to_string();
        match move_to_delivery(path, parent, &delivery_id, file_name, false) {
            Ok(destination) => return Ok(destination),
            Err(error) => {
                last_error = Some(error);
                if !path.exists() {
                    anyhow::bail!(
                        "delivery source disappeared while assigning identity: {}",
                        path.display()
                    );
                }
                if attempt + 1 < DELIVERY_MOVE_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("could not assign a delivery identity")))
}

fn move_to_delivery(
    source: &Path,
    parent: &Path,
    delivery_id: &str,
    original_name: &str,
    allow_idempotent_existing: bool,
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        is_delivery_dir_name(&format!("{DELIVERY_DIR_PREFIX}{delivery_id}")),
        "invalid delivery identifier"
    );
    anyhow::ensure!(
        !original_name.is_empty()
            && !original_name.contains('/')
            && !original_name.contains('\\')
            && original_name != "."
            && original_name != "..",
        "invalid original filename in delivery envelope"
    );

    let delivery_dir = parent.join(format!("{DELIVERY_DIR_PREFIX}{delivery_id}"));
    match std::fs::create_dir(&delivery_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::ensure!(delivery_dir.is_dir(), "delivery path is not a directory");
        }
        Err(error) => return Err(error.into()),
    }

    let destination = delivery_dir.join(original_name);
    if destination.exists() {
        if allow_idempotent_existing && same_file_content(source, &destination)? {
            std::fs::remove_file(source)?;
            return Ok(destination);
        }
        anyhow::bail!(
            "delivery {} already contains different content for {}",
            delivery_id,
            original_name
        );
    }

    match std::fs::rename(source, &destination) {
        Ok(()) => Ok(destination),
        Err(error) => {
            if delivery_dir.read_dir().is_ok_and(|mut entries| entries.next().is_none()) {
                let _ = std::fs::remove_dir(&delivery_dir);
            }
            Err(error.into())
        }
    }
}

fn parse_incoming_file_name(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix(INCOMING_FILE_PREFIX)?;
    let separator = rest.find("__")?;
    let token = &rest[..separator];
    let original_name = &rest[separator + 2..];
    if token.is_empty()
        || token.len() > 160
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        || original_name.is_empty()
        || original_name.contains('/')
        || original_name.contains('\\')
    {
        return None;
    }
    Some((token.to_string(), original_name.to_string()))
}

fn delivery_id_from_source_token(token: &str) -> String {
    let digest = hex::encode(Sha256::digest(token.as_bytes()));
    digest[..DELIVERY_ID_HEX_LEN].to_string()
}

fn same_file_content(left: &Path, right: &Path) -> anyhow::Result<bool> {
    let left_metadata = std::fs::metadata(left)?;
    let right_metadata = std::fs::metadata(right)?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }
    Ok(file_digest(left)? == file_digest(right)?)
}

fn file_digest(path: &Path) -> anyhow::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn is_delivery_dir_name(name: &str) -> bool {
    let Some(identifier) = name.strip_prefix(DELIVERY_DIR_PREFIX) else {
        return false;
    };
    identifier.len() == DELIVERY_ID_HEX_LEN
        && identifier.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn same_content_and_normalized_path_produce_same_instance_id() {
        let first = instance_id(SHA, &normalize_relpath(r"Folder\Contract.pdf"));
        let second = instance_id(SHA, &normalize_relpath("folder/./contract.pdf"));
        assert_eq!(first, second);
    }

    #[test]
    fn same_content_at_different_paths_produces_different_instance_ids() {
        let first = instance_id(SHA, &normalize_relpath("one/contract.pdf"));
        let second = instance_id(SHA, &normalize_relpath("two/contract.pdf"));
        assert_ne!(first, second);
    }

    #[test]
    fn instance_ids_are_lowercase_ascii_hex() {
        let id = instance_id(SHA, &normalize_relpath("dansk/Årsrapport.pdf"));
        assert!(is_safe_identifier(&id));
    }

    #[test]
    fn normalization_collapses_parent_segments_without_losing_unicode() {
        assert_eq!(
            normalize_relpath(r"KUNDER\Acme\..\Málaga\AFTALE.PDF"),
            "kunder/málaga/aftale.pdf"
        );
    }

    #[test]
    fn unsafe_manifest_identifiers_are_rejected() {
        assert!(!is_safe_identifier("../manifest"));
        assert!(!is_safe_identifier(&"A".repeat(64)));
        assert!(!is_safe_identifier(&"f".repeat(63)));
    }

    #[test]
    fn unwrapped_arrival_is_moved_without_changing_its_filename() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("Shareholder Register.pdf");
        std::fs::write(&source, b"fixture").unwrap();

        let wrapped = ensure_delivery_path(&source).unwrap();

        assert!(!source.exists());
        assert!(wrapped.exists());
        assert_eq!(wrapped.file_name().unwrap(), "Shareholder Register.pdf");
        assert!(has_delivery_identity(&wrapped));
    }

    #[test]
    fn later_same_name_and_content_receives_a_new_instance_path() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("Agreement.pdf");
        std::fs::write(&source, b"same bytes").unwrap();
        let first = ensure_delivery_path(&source).unwrap();

        std::fs::write(&source, b"same bytes").unwrap();
        let second = ensure_delivery_path(&source).unwrap();

        assert_ne!(first, second);
        let first_id = instance_id(SHA, &normalize_relpath(&first.to_string_lossy()));
        let second_id = instance_id(SHA, &normalize_relpath(&second.to_string_lossy()));
        assert_ne!(first_id, second_id);
    }

    #[test]
    fn power_automate_envelope_is_stable_and_restores_original_name() {
        let directory = tempfile::tempdir().unwrap();
        let incoming = directory
            .path()
            .join("__incoming_flow123-42__Agreement.pdf");
        std::fs::write(&incoming, b"fixture").unwrap();

        let delivery = ensure_delivery_path(&incoming).unwrap();

        assert_eq!(delivery.file_name().unwrap(), "Agreement.pdf");
        assert!(has_delivery_identity(&delivery));
    }

    #[test]
    fn retried_power_automate_delivery_is_an_idempotent_no_op() {
        let directory = tempfile::tempdir().unwrap();
        let incoming = directory
            .path()
            .join("__incoming_flow123-42__Agreement.pdf");
        std::fs::write(&incoming, b"same bytes").unwrap();
        let first = ensure_delivery_path(&incoming).unwrap();

        std::fs::write(&incoming, b"same bytes").unwrap();
        let replay = ensure_delivery_path(&incoming).unwrap();

        assert_eq!(first, replay);
        assert!(!incoming.exists());
    }

    #[test]
    fn conflicting_power_automate_replay_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let incoming = directory
            .path()
            .join("__incoming_flow123-42__Agreement.pdf");
        std::fs::write(&incoming, b"first bytes").unwrap();
        ensure_delivery_path(&incoming).unwrap();

        std::fs::write(&incoming, b"different bytes").unwrap();
        assert!(ensure_delivery_path(&incoming).is_err());
        assert!(incoming.exists());
    }

    #[test]
    fn existing_delivery_path_is_reused() {
        let directory = tempfile::tempdir().unwrap();
        let delivery = directory
            .path()
            .join(format!("{DELIVERY_DIR_PREFIX}{}", "a".repeat(32)));
        std::fs::create_dir(&delivery).unwrap();
        let file = delivery.join("Agreement.pdf");
        std::fs::write(&file, b"fixture").unwrap();

        assert_eq!(ensure_delivery_path(&file).unwrap(), file);
    }
}
