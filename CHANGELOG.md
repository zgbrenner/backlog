# Changelog

Notable changes to BackLog. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow the
single number that `package.json`, `src-tauri/tauri.conf.json` and
`src-tauri/Cargo.toml` must agree on (CI enforces this — see
`.github/scripts/check-versions.mjs`).

`RELEASING.md` step "Cutting an updating release" references this file: the
release notes and the `notes` field of `latest.json` should quote the section
below for the version being cut.

> **Provenance.** The 0.2.0 section was reconstructed from the working tree and
> the superseded `PRODUCTION_READINESS.md` rather than from a per-commit log,
> because the pre-0.2.0 history was squashed. Treat it as an accurate summary
> of *what the code does now*, not as a commit-by-commit record.

## [Unreleased]

### Added
- `.github/workflows/ci.yml`: five Linux jobs (trust core, workspace, frontend
  + UI harness, Python 3.11, release version agreement). The project previously
  had no CI at all, and `cargo fmt --all -- --check` — an item
  `docs/RELEASE_CHECKLIST.md` calls mandatory — had never been executed.
- `rust-toolchain.toml` pinning the channel with `rustfmt` and `clippy`.
- `LICENSE` (proprietary, source-available) and `NOTICE.md` enumerating every
  redistributed component, so the redistribution gate in
  `docs/DEPENDENCY_COMPATIBILITY.md` can be closed.
- `docs/USER_GUIDE.md`, `docs/TROUBLESHOOTING.md` and `docs/PRIVACY.md` — the
  first documents in the repo written for the office worker who runs this
  appliance rather than for a builder or auditor.
- `docs/KNOWN_ISSUES.md` and `docs/DECISIONS.md`, replacing
  `PRODUCTION_READINESS.md`.
- `scripts/verify-binaries.ps1`: refuses to package a release whose sidecars
  are dev stubs, truncated builds, or not valid PE images.
- `training/README.md` and `training/requirements.txt`.
- `bundle.windows.webviewInstallMode: offlineInstaller` and
  `bundle.windows.nsis.installMode: currentUser` in `tauri.conf.json`.

### Changed
- `scripts/dev-stubs.{sh,ps1}` write a `BACKLOG-DEV-STUB-DO-NOT-SHIP` marker
  instead of zero bytes, so a stub is provably a stub.
- `power-automate/manifest.schema.json`,
  `power-automate/manifest.parse-json.schema.json` and the example fixtures
  move to manifest **v3** (`dismissed` status; `model_versions` required
  non-empty on `ok`), matching `src-tauri/src/manifest.rs`.
- `power-automate/FLOW1-intake.md` rewritten: Flow 1 now delivers each file
  under its plain original name inside a per-delivery subfolder it composes
  itself. The previously documented `__incoming_<flow-id>-<item-id>__` envelope
  described app behavior that does not exist.

### Fixed
- README behavior guarantees, prerequisites and setup steps that did not match
  the code (`--vl`, "Python 3.11+", the llama-server version rationale,
  "indexed once", "never deleted").

## [0.2.0] — backend hardening + licensing-clean model swap

### Added
- **Encryption at rest for the ledger.** `ledger.db` is whole-file encrypted
  with SQLCipher (`rusqlite`'s `bundled-sqlcipher-vendored-openssl`). The
  256-bit key is generated on first open, DPAPI-protected, and stored at
  `<data_dir>/ledger.key` — never written in plaintext (`src-tauri/src/dbkey.rs`).
- **In-app model downloader** (`src-tauri/src/model_download.rs` +
  `download_models` command): resumable, cancellable, SHA-256-verified against
  `models.lock.json`, so a non-technical user never opens a terminal to install
  the two Qwen3 GGUFs.
- **Runtime preflight** (`src-tauri/src/preflight.rs`): every check carries a
  plain-language `message`, a technical `detail`, and — where the app can fix
  it — an `action` the UI renders as a button.
- **Signed auto-updater**: `tauri-plugin-updater` with a minisign pubkey in
  `tauri.conf.json`, checked once at startup by `src/main.ts`.
- **System tray + close-to-hide**: closing the window no longer quits the
  process and kills the sidecars mid-batch.
- `JobState::Dismissed` and the `dismiss` command: a terminal human decision,
  distinct from `Emitted` so throughput does not count it as work delivered.
- Diagnostics (`get_diagnostics`), structured logging with path redaction
  (`src-tauri/src/logging.rs`), and ledger read APIs behind the review loop.
- `backlog-core` crate: `harvest` + `checker` extracted with no Tauri, sidecar
  or icon dependency, so the trust core tests on a bare checkout — which is
  what makes Linux CI possible at all.

### Changed
- **Licensing-clean model swap**: Qwen3-0.6B/1.7B, Lingua and RapidOCR 3
  replace LFM2.5, fastText `lid.176` and `rapidocr-onnxruntime`.
- **Slim, torch-free sidecar**: `torch`, `transformers`,
  `sentence-transformers` and `gliclass` removed (~3x smaller Python footprint).
  `classify`, `salience` and `ettin_spans` degrade to deterministic `ok=true`
  fallbacks with `available: false`, so no document is ever flagged over a
  missing naming enhancement.
- Manifest schema **v2 → v3**: adds the `dismissed` status and requires a
  non-empty `model_versions` on an `ok` manifest.
- Model paths are rehomed at startup to `%APPDATA%\ai.sonomos.backlog\models`;
  a path set through Settings' Browse dialog passes through untouched.
- Retry ladder rungs now vary the *evidence bundle*, not just the model tier —
  rung 3 used to be byte-identical to rung 1.

### Fixed
- **Duplicate path.** The duplicate manifest id was `{sha}:{uuid}`: `:` is
  invalid on NTFS so the write silently failed on Windows, and the fresh UUID
  made replay non-idempotent. Now a deterministic filesystem-safe per-copy key,
  with a durable ledger row per physical copy so `(2)`/`(3)` names increment.
- **Sidecar could wedge the pipeline.** stdout is drained on a reader thread
  with an enforced per-request deadline (kill + respawn on timeout), and `Drop`
  guarantees no orphaned sidecar processes.
- **Crash-loop guard**: a durable attempt counter quarantines a poison-pill
  document as `CRASH_LOOP` after 5 restarts.
- **Watcher no longer skips leading-underscore filenames.** `_DRAFT
  Agreement.docx` was silently dropped — no ledger row, no manifest, no log.
- Unicode crash in `harvest()`; `tail_pages=0` trim no-op; pdfium bitmap leak;
  `get_evidence` path traversal; PII in the persisted `events` table; unbounded
  header read during type detection.
- Removed `tauri-plugin-shell` and `tauri-plugin-opener` and their capability
  grants — all dead code, and pure IPC attack surface.
