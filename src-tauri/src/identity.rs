//! Stable, filesystem-safe identifiers for physical file instances and the
//! manifests that describe them.
//!
//! A content SHA-256 identifies bytes. An instance/manifest id identifies those
//! bytes at one delivery path, so identical content dropped as separate files
//! can be delivered (and named " (2)", " (3)") independently without
//! overloading the content hash or using replay-unsafe randomness. The id is
//! deliberately 64 lowercase-hex — filesystem-safe, unlike the old `{sha}:{uuid}`
//! form whose `:` was rejected by NTFS.

use sha2::{Digest, Sha256};

/// Normalize a relative path for identity: forward slashes, lowercased, with
/// `.`/`..` resolved. Windows/SharePoint treat neither separator nor case as
/// meaningful; the original path is still preserved verbatim in the ledger and
/// manifest.
pub fn normalize_relpath(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in unified.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/").to_lowercase()
}

/// Deterministic 64-hex identity for one physical file instance: the content
/// hash bound to its normalized delivery path. Same copy -> same id (idempotent
/// replay); distinct copies -> distinct ids (each gets its own manifest + row).
pub fn instance_id(content_sha256: &str, normalized_relpath: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content_sha256.trim().to_ascii_lowercase().as_bytes());
    hasher.update([0]);
    hasher.update(normalized_relpath.as_bytes());
    hex::encode(hasher.finalize())
}

/// A manifest/instance id or content SHA is exactly 64 lowercase hex chars.
pub fn is_safe_identifier(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_is_64_hex_and_fs_safe() {
        let id = instance_id(&"a".repeat(64), "sub/dir/file.pdf");
        assert!(is_safe_identifier(&id));
        assert!(!id.contains([':', '\\', '/', '*', '?']));
    }

    #[test]
    fn instance_id_is_deterministic_and_path_sensitive() {
        let sha = "b".repeat(64);
        assert_eq!(
            instance_id(&sha, "a/one.pdf"),
            instance_id(&sha, "a/one.pdf")
        );
        assert_ne!(
            instance_id(&sha, "a/one.pdf"),
            instance_id(&sha, "a/two.pdf")
        );
    }

    #[test]
    fn normalize_relpath_unifies_separators_and_case() {
        assert_eq!(normalize_relpath("Sub\\Dir\\File.PDF"), "sub/dir/file.pdf");
        assert_eq!(normalize_relpath("./a/../b/c"), "b/c");
    }

    #[test]
    fn rejects_unsafe_identifiers() {
        assert!(!is_safe_identifier("short"));
        assert!(!is_safe_identifier(&"g".repeat(64))); // non-hex
        assert!(!is_safe_identifier(&format!(
            "{}-{}",
            "a".repeat(47),
            "b".repeat(16)
        )));
    }
}
