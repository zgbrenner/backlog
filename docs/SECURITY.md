# Security and privacy posture

BackLog processes potentially sensitive documents. Runtime conversion,
language detection, classification, embeddings, and Qwen inference are designed
to remain on the local machine. SharePoint receives the committed document and
limited index metadata only through explicitly configured Power Automate flows.

## Enforced runtime invariants

- The Python sidecar forces Hugging Face Hub, Transformers, and Datasets offline
  modes before importing model libraries.
- Rust injects the verified installed model resource directory into
  `BACKLOG_MODELS_DIR` before starting the sidecar.
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
- Runtime model downloads are not implemented.

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

The SQLite ledger, converted Markdown cache, quarantine directory, Processing
and Processed directories, model files, and logs must be protected by operating
system access controls and full-disk encryption. Retention for cache,
quarantine, review evidence, and backups must follow the organization's data
retention policy.

Do not place real customer or Vistage documents, personal data, model weights,
API credentials, signing material, tenant exports, or production configuration
inside GitHub issues, CI logs, fixtures, screenshots, or release artifacts.

## Reporting

For this private project, report a suspected vulnerability directly to the
repository owner. Preserve the affected commit, installer and model hashes,
configuration, Power Automate versions, and local logs required to reproduce
the problem without uploading document content.
