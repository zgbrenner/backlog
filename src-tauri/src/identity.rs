//! Stable identifiers for physical file instances and emitted manifests.
//!
//! A content SHA-256 identifies bytes. An instance ID identifies those bytes at
//! one normalized path, so identical content can be processed separately without
//! overloading the content hash or introducing replay-unsafe randomness.

use sha2::{Digest, Sha256};

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

/// Derive a stable physical-file identifier from true content identity and path.
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
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn same_content_and_normalized_path_produce_same_instance_id() {
        let a = instance_id(SHA, &normalize_relpath(r"Folder\Contract.pdf"));
        let b = instance_id(SHA, &normalize_relpath("folder/./contract.pdf"));
        assert_eq!(a, b);
    }

    #[test]
    fn same_content_at_different_paths_produces_different_instance_ids() {
        let a = instance_id(SHA, &normalize_relpath("one/contract.pdf"));
        let b = instance_id(SHA, &normalize_relpath("two/contract.pdf"));
        assert_ne!(a, b);
    }

    #[test]
    fn instance_ids_are_lowercase_ascii_hex() {
        let id = instance_id(SHA, &normalize_relpath("dansk/Årsrapport.pdf"));
        assert!(is_safe_identifier(&id));
        assert!(id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
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
}
