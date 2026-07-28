# Security and privacy posture

BackLog processes potentially sensitive documents. Runtime conversion,
language detection, classification, embeddings, and Qwen inference are designed
to remain on the local machine. SharePoint receives the committed document and
limited index metadata only through explicitly configured Power Automate flows.

`docs/PRIVACY.md` is this same posture written for the office worker who runs
the appliance. Where the two differ, this file is the more precise one.

## Enforced runtime invariants

- The Python sidecar forces Hugging Face Hub, Transformers, and Datasets offline
  modes before importing model libraries.
- Rust injects the app-data models directory (`app_data_dir()/models`, i.e.
  `%APPDATA%\ai.sonomos.backlog\models`) into `BACKLOG_MODELS_DIR` before
  starting the sidecar. Models are **not** installer resources: `bundle.resources`
  maps only `resources/*` and `binaries/*.dll`, and `lib.rs` rehomes both GGUF
  paths to that directory at startup — which is also where the in-app downloader
  writes. An absolute path a user set via Settings' Browse dialog is honored
  untouched.
- llama-server binds only to `127.0.0.1` and uses two dedicated local ports.
- The deterministic Rust checker is the final authority before an `ok` manifest.
- A source document is never silently deleted. Failure becomes terminal only
  after a durable flagged manifest exists.
- Content SHA-256, physical InstanceId, and ManifestId are separate identities.
- Corrected manifests reuse the same physical ManifestId so Flow 2 can preserve
  an audit trail instead of treating the correction as an unrelated document.
- Power Automate error rows contain operational details, not extracted document
  text or evidence bundles.
- The desktop capability file grants only Tauri core, event, window, and dialog
  permissions. The application does not initialize shell or opener plugins.
- Two plugins ARE initialized and are deliberate grants, not oversights:
  - **`tauri-plugin-updater`** — the signed self-update channel. It is the one
    component permitted to download and execute code. Every update is verified
    against the minisign public key embedded in `tauri.conf.json`
    (`plugins.updater.pubkey`) before install, so an attacker who controls the
    release asset still cannot ship one; losing the private key ends the update
    chain rather than compromising it. `src/main.ts`'s `checkForUpdates` runs
    once at startup and swallows failures on purpose (a failed check must not
    interrupt the user) — which is why `RELEASING.md` step 4 asserts the
    endpoint from the build machine instead.
  - **`tauri-plugin-process`** — supplies only the `relaunch()` the frontend
    calls after a successful update install. It grants no process spawning; the
    sidecars are started from Rust with `std::process`.
- The updater installs passively and the NSIS package installs per-user
  (`nsis.installMode: currentUser`), so a background update never raises a UAC
  prompt the appliance user cannot satisfy. `webviewInstallMode` is
  `offlineInstaller`, so no part of installation contacts Microsoft.
- Model weights are one of exactly two deliberate exceptions to "no runtime
  network access" (the other is the updater check above): `download_models` (`src-tauri/src/model_download.rs`), invoked
  once from Settings when Readiness reports missing model files, streams the
  pinned Hugging Face repos over HTTPS into `app_data/models` and SHA-256-
  verifies every file against `models.lock.json` (trust-on-first-download,
  then verify on every subsequent run — mirroring `models/download_models.py`).
  This uses the `reqwest` crate directly from Rust, not a webview-exposed
  plugin, so it added no capability grant. Document processing itself never
  reaches the network.

## Required release controls

- Hash-pin llama-server, the model-bundle ZIP, every model payload, the frozen
  sidecar, and the installer.
- Generate and retain software and model bills of materials.
- Use a hash-pinned `sidecar/requirements.lock` for any signed release.
- Code-sign the installer and external executables before deployment beyond the
  internal pilot.
- Run anti-malware scanning on every bundled executable.
- Observe runtime traffic with a connection monitor and confirm only loopback
  communication while document processing runs.
- Validate install, repair, upgrade, and uninstall under the actual Windows
  deployment policy.

## Sensitive local artifacts

Everything lives under `%APPDATA%\ai.sonomos.backlog` except the quarantine
folder, which the operator chooses.

| Artifact | Protection |
|---|---|
| `ledger.db` — original names, proposed dates, subjects, descriptions, local paths, event trail | Whole-file SQLCipher encryption (`rusqlite` `bundled-sqlcipher-vendored-openssl`). |
| `ledger.key` — the 256-bit SQLCipher key | DPAPI `CryptProtectData`, decryptable only by the same Windows user on the same machine. Never written in plaintext. |
| `cache\*.md` — converted document text | Purged on emit (`retain_cache=false` default); flagged files keep theirs until review resolves; `cache_ttl_days` (7) sweeps orphans at startup. |
| `<outbox>\_manifests\*.json` | **Not encrypted, by necessity** — Power Automate must read them. They carry the proposed filename, description, date and identifiers, never document text, and Flow 2 deletes each after committing. |
| Quarantine directory | Unmodified source documents. Protect it as you protect the originals; it must be local, not synced. |
| `logs\` | Folder paths reduced to drive + depth (`logging::redact_path`); model output replaced with `[model output withheld]`. |
| `models\` | ~2.4 GB of Apache-2.0 weights. Not sensitive; SHA-256-verified against `models.lock.json`. |

All of the above still require operating-system access controls and full-disk
encryption. Retention for cache, quarantine, review evidence, and backups must
follow the organization's data retention policy; note that nothing currently
prunes the ledger's `events` table (`docs/KNOWN_ISSUES.md` item 5).

## Uninstall residue

The NSIS uninstaller removes the installed program. It deliberately leaves
**all** user data behind, because deleting it silently would destroy the record
of what was filed and force a 2.4 GB re-download:

- `%APPDATA%\ai.sonomos.backlog` survives uninstall — the encrypted ledger, the
  DPAPI key blob, the converted-text cache, `backlog.config.json`, the logs, and
  the model weights.
- The operator-chosen **quarantine folder** survives uninstall and holds
  unmodified customer documents. BackLog does not delete a document under any
  circumstances, including its own removal.
- Processing and Outbox are OneDrive folders BackLog only read from and wrote
  manifests into; it removes nothing there.

Decommissioning a machine therefore means deleting
`%APPDATA%\ai.sonomos.backlog` **and** the quarantine folder explicitly.
Deleting `ledger.key` renders the ledger permanently unreadable, which is the
intended effect and is not reversible. This is stated in plain language for the
end user in `docs/PRIVACY.md`.

Do not place real customer or Vistage documents, personal data, model weights,
API credentials, signing material, tenant exports, or production configuration
inside GitHub issues, local build logs, fixtures, screenshots, or release
artifacts.

## Reporting

For this private project, report a suspected vulnerability directly to the
repository owner. Preserve the affected commit, installer and model hashes,
configuration, Power Automate versions, and local logs required to reproduce
the problem without uploading document content.
