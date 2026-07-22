# Security and privacy posture

BackLog processes potentially sensitive documents. Runtime inference and
conversion are designed to remain on the local machine. SharePoint receives the
committed document and limited index metadata only through explicitly configured
Power Automate flows.

## Invariants

- The Python sidecar forces Hugging Face and Transformers offline modes.
- Rust launches the sidecar with an explicit local model directory.
- llama-server binds to `127.0.0.1`.
- The deterministic checker is the final authority before an `ok` manifest.
- The app does not delete a source document.
- Cloud error lists must never contain extracted document text or evidence.
- Tauri grants only the capabilities used by the UI.

## Sensitive local artifacts

The SQLite ledger, converted Markdown cache, quarantine folder, model files,
and logs should be protected by operating-system access controls and full-disk
encryption. Cache and quarantine retention must follow the organization's data
retention policy.

## Reporting

For this private project, report a suspected vulnerability directly to the
repository owner and avoid placing real document samples, secrets, or personal
data in a GitHub issue. Preserve the affected commit, configuration, and local
logs needed to reproduce the problem.
