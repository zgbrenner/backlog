//! Stable identifiers for physical file instances and emitted manifests.
//!
//! A content SHA-256 identifies bytes. An instance ID identifies those bytes at
//! one durable delivery path, so identical content can be processed separately
//! without overloading the content hash or introducing replay-unsafe randomness.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DELIVERY_DIR_PREFIX: &str = "__bl_";
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
        .filter_map(Path::file_name)
        .filter_map(|name| name.to_str())
        .any(is_delivery_dir_name)
}

/// Move an unwrapped arrival into a unique sibling delivery directory while
/// preserving its human filename. The move happens before hashing and ledger
/// registration. A crash after the move is safe because the delivery directory
/// persists and the startup sweep discovers it again.
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
        .ok_or_else(|| anyhow::anyhow!("delivery source has no filename: {}", path.display()))?;

    let mut last_error: Option<std::io::Error> = None;
    for attempt in 0..DELIVERY_MOVE_ATTEMPTS {
        let delivery_id = uuid::Uuid::new_v4().simple().to_string();
        let delivery_dir = parent.join(format!("{DELIVERY_DIR_PREFIX}{delivery_id}"));
        match std::fs::create_dir(&delivery_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }

        let destination = delivery_dir.join(file_name);
        match std::fs::rename(path, &destination) {
            Ok(()) => return Ok(destination),
            Err(error) => {
                last_error = Some(error);
                let _ = std::fs::remove_dir(&delivery_dir);
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

    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("could not assign a delivery identity")))
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
