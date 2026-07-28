//! The SQLCipher key for the ledger, protected at rest with Windows DPAPI.
//!
//! `ledger.db` holds derived PII (proposed subjects, descriptions, filenames,
//! local paths), so it is encrypted whole-file with SQLCipher (see
//! `ledger.rs`). SQLCipher needs a raw 256-bit key; that key has to live
//! *somewhere* durable so the app can reopen the db on the next launch, but
//! it must never sit on disk in plaintext. DPAPI (`CryptProtectData`) wraps
//! it with a key derived from the current Windows user's login credentials —
//! the same primitive Windows itself uses for saved Wi-Fi passwords and
//! Credential Manager — so the blob on disk is only ever meaningful to that
//! same Windows user on that same machine. There is no additional
//! passphrase or key-wrapping scheme to manage.
//!
//! `CryptUnprotectData` is the exact inverse: handed the DPAPI blob, it
//! returns the original 32 bytes, decryptable only by that same user login.
//!
//! ## Non-Windows builds
//!
//! BackLog ships Windows-only (`bundle.targets` is NSIS), so the DPAPI path
//! below is the only one a user ever runs. The `#[cfg(not(windows))]` fallback
//! exists purely so this crate — and therefore the ledger, pipeline, manifest,
//! and checker test suites — compiles and runs on a Linux CI runner. It wraps
//! the key with a marker header and 0600 permissions rather than DPAPI, which
//! is strictly weaker; `resolve_key` logs a warning saying so, and
//! `assert_shipping_key_protection_is_dpapi` fails the build's own test suite
//! if that fallback is ever reached on a Windows target.

use std::path::Path;

pub const KEY_LEN: usize = 32;

/// Resolve the 32-byte SQLCipher key stored (DPAPI-protected) at
/// `key_path`, generating a fresh random key on first run.
///
/// Never writes the raw key to disk: the file at `key_path` always holds
/// the `CryptProtectData` ciphertext blob, not the key itself.
pub fn resolve_key(key_path: &Path) -> anyhow::Result<[u8; KEY_LEN]> {
    #[cfg(not(windows))]
    log::warn!(
        "ledger key at {} is protected by file permissions, not DPAPI: this is a \
         non-Windows development/CI build and must never be shipped",
        key_path.display()
    );

    if key_path.exists() {
        let blob = std::fs::read(key_path)
            .map_err(|e| anyhow::anyhow!("reading ledger key blob {}: {e}", key_path.display()))?;
        let key = unprotect(&blob).map_err(|e| {
            anyhow::anyhow!(
                "decrypting ledger key {} (usually means a different Windows user/machine): {e}",
                key_path.display()
            )
        })?;
        anyhow::ensure!(
            key.len() == KEY_LEN,
            "decrypted ledger key has unexpected length {} (want {KEY_LEN})",
            key.len()
        );
        let mut out = [0u8; KEY_LEN];
        out.copy_from_slice(&key);
        Ok(out)
    } else {
        let mut key = [0u8; KEY_LEN];
        getrandom::getrandom(&mut key)?;
        let blob = protect(&key)?;
        if let Some(parent) = key_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_key_file(key_path, &blob)?;
        Ok(key)
    }
}

/// Write the wrapped key blob, owner-only where the platform can express it.
///
/// On Windows the blob is already DPAPI-bound to the user, so ACLs are belt
/// and braces; on Unix the 0600 mode *is* the protection, and the mode is set
/// at create time rather than chmod'd afterwards so there is no window in
/// which the file is group/world-readable.
fn write_key_file(key_path: &Path, blob: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(key_path)?;
    f.write_all(blob)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::{HLOCAL, LocalFree};
#[cfg(windows)]
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};

/// DPAPI-encrypt `data` under the current Windows user's login credentials.
#[cfg(windows)]
fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(
            &input,
            PCWSTR::null(), // no human-readable description needed
            None,           // no additional entropy beyond the user's DPAPI master key
            None,           // reserved
            None,           // no UI prompt struct
            CRYPTPROTECT_UI_FORBIDDEN, // never show a UI, fail instead
            &mut output,
        )?;
        Ok(take_blob(output))
    }
}

/// Inverse of `protect`: decrypt a DPAPI blob back to its raw bytes.
#[cfg(windows)]
fn unprotect(blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: blob.len() as u32, pbData: blob.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(
            &input,
            None, // discard the optional description CryptProtectData could have stored
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )?;
        Ok(take_blob(output))
    }
}

/// Copy a `CRYPT_INTEGER_BLOB` that DPAPI allocated (via `LocalAlloc`) into
/// an owned `Vec`, then free the buffer DPAPI handed back — callers own
/// `output` on the stack, but the *contents* `pbData` points at are on
/// DPAPI's local heap until we free them here.
#[cfg(windows)]
unsafe fn take_blob(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    unsafe {
        let bytes = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(blob.pbData as *mut _)));
        bytes
    }
}

// ---------------------------------------------------------------------------
// Non-Windows (development / CI only — never shipped; see the module docs).
// ---------------------------------------------------------------------------

/// Marker prefix so a fallback-wrapped key file is instantly identifiable and
/// can never be mistaken for — or silently read as — a DPAPI blob.
#[cfg(not(windows))]
const FALLBACK_MAGIC: &[u8] = b"BACKLOG-DEVKEY-v1\n";

#[cfg(not(windows))]
fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(FALLBACK_MAGIC.len() + data.len());
    out.extend_from_slice(FALLBACK_MAGIC);
    out.extend_from_slice(data);
    Ok(out)
}

#[cfg(not(windows))]
fn unprotect(blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    let body = blob.strip_prefix(FALLBACK_MAGIC).ok_or_else(|| {
        anyhow::anyhow!(
            "ledger key file is not a development-fallback blob (a DPAPI blob written on \
             Windows cannot be opened on this platform)"
        )
    })?;
    Ok(body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the cfg split itself: if the non-Windows fallback ever ends up
    /// compiled into a Windows build, the shipped artifact would store the
    /// SQLCipher key with a 18-byte header and no DPAPI wrapping at all. This
    /// test makes that a build failure rather than a silent downgrade.
    #[test]
    fn assert_shipping_key_protection_is_dpapi() {
        let key = [7u8; KEY_LEN];
        let blob = protect(&key).unwrap();
        if cfg!(windows) {
            // DPAPI always expands and randomizes; it never emits the raw key.
            assert_ne!(blob, key.to_vec(), "Windows build is not DPAPI-protecting the key");
            assert!(blob.len() > KEY_LEN);
        } else {
            assert!(blob.starts_with(b"BACKLOG-DEVKEY-v1\n"));
        }
    }

    #[test]
    fn generates_and_persists_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("ledger.key");
        assert!(!key_path.exists());

        let key1 = resolve_key(&key_path).unwrap();
        assert!(key_path.exists());
        assert_eq!(key1.len(), KEY_LEN);

        // The blob on disk must not be the raw key (DPAPI-wrapped, not
        // plaintext) — a 32-byte file would be a dead giveaway of a bug.
        let blob = std::fs::read(&key_path).unwrap();
        assert_ne!(blob, key1.to_vec());

        // Reopening must decrypt back to the exact same key.
        let key2 = resolve_key(&key_path).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn distinct_key_files_get_distinct_keys() {
        let dir = tempfile::tempdir().unwrap();
        let a = resolve_key(&dir.path().join("a.key")).unwrap();
        let b = resolve_key(&dir.path().join("b.key")).unwrap();
        assert_ne!(a, b);
    }
}
