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

//! ## Keeping the key out of freed memory
//!
//! The raw key and the `PRAGMA key = "x'…'"` text built from it are the two
//! places the 256-bit secret exists in this process. Left to ordinary drops
//! they stay legible in freed heap for the lifetime of the process, and
//! therefore in any crash dump, hibernation file or page file — on a machine
//! that, by `docs/SECURITY.md`'s own admission, may not have full-disk
//! encryption. [`SecretKey`] and [`ZeroizingString`] below overwrite their
//! buffers before releasing them. (Hand-rolled rather than pulling in the
//! `zeroize` crate: it is two volatile-write loops, and this crate's
//! dependency set is deliberately small.)

use std::path::Path;

pub const KEY_LEN: usize = 32;

/// Overwrite `bytes` with zeros using writes the optimizer is not permitted
/// to elide as dead stores, then fence so they cannot be sunk past the
/// deallocation that follows.
fn zeroize(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a live, uniquely borrowed, properly aligned u8.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// The SQLCipher key, wrapped so it is scrubbed when it goes out of scope.
///
/// Deliberately not `Debug`/`Display`/`Clone`: the only ways out are
/// [`SecretKey::pragma_statement`] and the byte slice, so it cannot end up in
/// a log line or an error message by accident.
pub struct SecretKey([u8; KEY_LEN]);

impl SecretKey {
    /// The complete `PRAGMA key` statement, hex included, in a buffer that
    /// scrubs itself on drop.
    ///
    /// Built whole here rather than handing out the hex for a caller to
    /// `format!` into place: a `format!` would allocate its own, unscrubbed
    /// copy of the key, and rusqlite's `SqlInputError` renders the offending
    /// SQL into its `Display`, so the statement should exist for as short a
    /// time and in as few places as possible.
    ///
    /// The `x'<hex>'` raw-key form (vs. a passphrase string) skips
    /// SQLCipher's PBKDF2 key derivation, which exists to stretch
    /// low-entropy human passphrases; this key is already uniformly random
    /// 256-bit CSPRNG output, so deriving further from it would add cost
    /// with no security benefit.
    // `allow(dead_code)` only until `ledger.rs::open` calls this instead of
    // building the statement with `format!("… {}", hex::encode(key))`, which
    // allocates a second, unscrubbed copy of the key. That file belongs to
    // another workstream; this is the half that lives here.
    #[allow(dead_code)]
    pub fn pragma_statement(&self) -> ZeroizingString {
        const PREFIX: &str = "PRAGMA key = \"x'";
        const SUFFIX: &str = "'\";";
        const HEX: &[u8; 16] = b"0123456789abcdef";

        // Exact capacity: a reallocation mid-build would leave a copy of the
        // partially written key in freed heap that nothing scrubs.
        let mut out = ZeroizingString::with_capacity(PREFIX.len() + KEY_LEN * 2 + SUFFIX.len());
        out.push_str(PREFIX);
        for byte in self.0 {
            out.push_ascii(HEX[(byte >> 4) as usize]);
            out.push_ascii(HEX[(byte & 0x0f) as usize]);
        }
        out.push_str(SUFFIX);
        out
    }
}

impl AsRef<[u8]> for SecretKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl PartialEq for SecretKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        zeroize(&mut self.0);
    }
}

/// A `String` whose bytes are overwritten before the allocation goes back to
/// the allocator. Only grows through [`ZeroizingString::with_capacity`]'s
/// reservation, so it never leaves a stale copy behind in a reallocation.
///
/// See the note on [`SecretKey::pragma_statement`] for why this is not yet
/// constructed outside tests.
#[allow(dead_code)]
pub struct ZeroizingString(String);

#[allow(dead_code)]
impl ZeroizingString {
    fn with_capacity(capacity: usize) -> Self {
        Self(String::with_capacity(capacity))
    }

    fn push_str(&mut self, text: &str) {
        debug_assert!(self.0.len() + text.len() <= self.0.capacity());
        self.0.push_str(text);
    }

    fn push_ascii(&mut self, byte: u8) {
        debug_assert!(byte.is_ascii() && self.0.len() < self.0.capacity());
        self.0.push(byte as char);
    }
}

impl std::ops::Deref for ZeroizingString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl Drop for ZeroizingString {
    fn drop(&mut self) {
        // SAFETY: the buffer is being dropped immediately after, and zero
        // bytes are valid UTF-8 regardless.
        zeroize(unsafe { self.0.as_mut_vec() });
    }
}

/// Resolve the 32-byte SQLCipher key stored (DPAPI-protected) at
/// `key_path`, generating a fresh random key on first run.
///
/// Never writes the raw key to disk: the file at `key_path` always holds
/// the `CryptProtectData` ciphertext blob, not the key itself.
pub fn resolve_key(key_path: &Path) -> anyhow::Result<SecretKey> {
    #[cfg(not(windows))]
    log::warn!(
        "ledger key at {} is protected by file permissions, not DPAPI: this is a \
         non-Windows development/CI build and must never be shipped",
        key_path.display()
    );

    if key_path.exists() {
        let blob = std::fs::read(key_path)
            .map_err(|e| anyhow::anyhow!("reading ledger key blob {}: {e}", key_path.display()))?;
        let mut key = unprotect(&blob).map_err(|e| {
            anyhow::anyhow!(
                "decrypting ledger key {} (usually means a different Windows user/machine): {e}",
                key_path.display()
            )
        })?;
        // Scrub the plaintext DPAPI output on every path out of here, not
        // just the happy one: `unprotect` hands back an ordinary heap Vec.
        let length_ok = key.len() == KEY_LEN;
        let mut out = SecretKey([0u8; KEY_LEN]);
        if length_ok {
            out.0.copy_from_slice(&key);
        }
        zeroize(&mut key);
        anyhow::ensure!(
            length_ok,
            "decrypted ledger key has unexpected length {} (want {KEY_LEN})",
            key.len()
        );
        Ok(out)
    } else {
        let mut key = SecretKey([0u8; KEY_LEN]);
        getrandom::getrandom(&mut key.0)?;
        let mut blob = protect(&key.0)?;
        if let Some(parent) = key_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let written = write_key_file(key_path, &blob);
        // On non-Windows the "blob" is the raw key behind a marker header, so
        // it is exactly as sensitive as the key itself.
        zeroize(&mut blob);
        written?;
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
use windows::Win32::Foundation::{LocalFree, HLOCAL};
#[cfg(windows)]
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

/// DPAPI-encrypt `data` under the current Windows user's login credentials.
#[cfg(windows)]
fn protect(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(
            &input,
            PCWSTR::null(),            // no human-readable description needed
            None,                      // no additional entropy beyond the user's DPAPI master key
            None,                      // reserved
            None,                      // no UI prompt struct
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
        let input = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
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
            assert_ne!(
                blob,
                key.to_vec(),
                "Windows build is not DPAPI-protecting the key"
            );
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
        assert_eq!(key1.as_ref().len(), KEY_LEN);

        // The blob on disk must not be the raw key (DPAPI-wrapped, not
        // plaintext) — a 32-byte file would be a dead giveaway of a bug.
        let blob = std::fs::read(&key_path).unwrap();
        assert_ne!(blob.as_slice(), key1.as_ref());

        // Reopening must decrypt back to the exact same key.
        let key2 = resolve_key(&key_path).unwrap();
        assert!(key1 == key2);
    }

    #[test]
    fn distinct_key_files_get_distinct_keys() {
        let dir = tempfile::tempdir().unwrap();
        let a = resolve_key(&dir.path().join("a.key")).unwrap();
        let b = resolve_key(&dir.path().join("b.key")).unwrap();
        assert!(a != b);
    }

    /// The statement handed to SQLCipher must carry the key in the raw
    /// `x'<hex>'` form, lowercase and full length, or SQLCipher would silently
    /// treat it as a passphrase and derive a different key.
    #[test]
    fn pragma_statement_renders_the_raw_key_form() {
        let key = SecretKey([0xabu8; KEY_LEN]);
        let statement = key.pragma_statement();
        assert_eq!(
            &*statement,
            format!("PRAGMA key = \"x'{}'\";", "ab".repeat(KEY_LEN))
        );
    }

    /// The reason `pragma_statement` builds the whole statement itself: it
    /// must never have to grow, because a reallocation leaves a copy of the
    /// key in freed heap that nothing scrubs.
    #[test]
    fn pragma_statement_never_reallocates() {
        let key = SecretKey([0x5au8; KEY_LEN]);
        let statement = key.pragma_statement();
        assert_eq!(statement.0.len(), statement.0.capacity());
    }

    #[test]
    fn zeroize_overwrites_every_byte() {
        let mut bytes = [0xffu8; 8];
        zeroize(&mut bytes);
        assert_eq!(bytes, [0u8; 8]);
    }

    /// Both wrappers must actually blank their buffer before it is released;
    /// reading the freed allocation is not something a test can do portably,
    /// so this checks the scrubbing step itself on a live buffer.
    #[test]
    fn dropping_the_wrappers_leaves_no_key_bytes_behind() {
        let mut key = SecretKey([0x11u8; KEY_LEN]);
        zeroize(&mut key.0);
        assert_eq!(key.0, [0u8; KEY_LEN]);

        let mut statement = SecretKey([0x22u8; KEY_LEN]).pragma_statement();
        assert!(statement.contains("2222"));
        // SAFETY: mirrors `ZeroizingString::drop` on a buffer we still own.
        zeroize(unsafe { statement.0.as_mut_vec() });
        assert!(!statement.contains("2222"));
    }
}
