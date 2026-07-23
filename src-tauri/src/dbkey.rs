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

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};

pub const KEY_LEN: usize = 32;

/// Resolve the 32-byte SQLCipher key stored (DPAPI-protected) at
/// `key_path`, generating a fresh random key on first run.
///
/// Never writes the raw key to disk: the file at `key_path` always holds
/// the `CryptProtectData` ciphertext blob, not the key itself.
pub fn resolve_key(key_path: &Path) -> anyhow::Result<[u8; KEY_LEN]> {
    if key_path.exists() {
        let blob = std::fs::read(key_path)
            .map_err(|e| anyhow::anyhow!("reading ledger key blob {}: {e}", key_path.display()))?;
        let key = dpapi_unprotect(&blob).map_err(|e| {
            anyhow::anyhow!(
                "DPAPI-decrypting ledger key {} (usually means a different Windows user/machine): {e}",
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
        let blob = dpapi_protect(&key)?;
        if let Some(parent) = key_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(key_path, &blob)?;
        Ok(key)
    }
}

/// DPAPI-encrypt `data` under the current Windows user's login credentials.
fn dpapi_protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
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

/// Inverse of `dpapi_protect`: decrypt a DPAPI blob back to its raw bytes.
fn dpapi_unprotect(blob: &[u8]) -> anyhow::Result<Vec<u8>> {
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
unsafe fn take_blob(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    unsafe {
        let bytes = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(blob.pbData as *mut _)));
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
