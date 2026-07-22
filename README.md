# BackLog

Local-first document naming and indexing pipeline. Watches a folder, converts
Word, PowerPoint, PDF, images, scans, and other business files to markdown, has
a small local language model propose `YYYY-MM-DD Subject` plus a one-sentence
description, validates every proposal with a deterministic checker, and hands
Power Automate a JSON manifest to rename, archive, and index the file in
SharePoint.

No cloud inference. No terminals. Every model runs on-device; the only cloud
surfaces are SharePoint and the two Power Automate flows.

Design doc: `2026-07-21 Sortition Pipeline Design.md` (the naming predates the
app name). Current dependency and release decisions are also recorded in
`docs/DEPENDENCY_COMPATIBILITY.md` and `docs/RELEASE_CHECKLIST.md`.

## Architecture at a glance

```
Intake (SharePoint) --Flow 1--> Processing folder (OneDrive-synced)
                                      |
                                  [BackLog app]
                Tauri shell, Rust core, SQLite ledger
                                      |
      convertd sidecar (Python): MarkItDown, pdfium, RapidOCR,
                 Lingua, GLiClass, Granite, optional Ettin
      llama-server sidecars: Qwen3-0.6B primary, Qwen3-1.7B escalation,
                 embedded chat templates + JSON Schema output
                                      |
                       deterministic checker (Rust)
                                      |
                     manifests -> Outbox/_manifests (synced)
                                      |
              Flow 2: rename, copy to Archive, index list row
              (flagged files -> local quarantine + NeedsReview list)
```

## Prerequisites

- Rust 1.97.1 and the Tauri 2 prerequisites for the target OS.
- Node 22 for the locked frontend build.
- 64-bit Python 3.11 for the frozen conversion sidecar and model staging.
- A reviewed `llama-server` build from llama.cpp that supports Qwen3 chat
  templates, `/v1/chat/completions`, and JSON Schema response formats.
- A complete BackLog model bundle whose files match `models/models.lock.json`.

## Setup

### 1. Stage and lock the models

On a connected staging machine:

```bash
cd models
python -m pip install huggingface_hub
python download_models.py
python download_models.py --verify-only
```

The core set is:

- `Qwen3-0.6B-Q8_0.gguf`, primary structured naming model;
- `Qwen3-1.7B-Q8_0.gguf`, escalation model;
- `gliclass-base-v3.0`, document-type classifier;
- `granite-embedding-small-english-r2`, salience model; and
- `ettin-encoder-32m`, training base only, not an enabled extractor.

The downloader writes `models.lock.json` with SHA-256 hashes. Commit and review
the lockfile, then create a ZIP containing the complete locked model directory
for Windows packaging. The app never downloads models at runtime.

### 2. Build the conversion sidecar

See `sidecar/BUILD.md`. It produces `convertd(.exe)`; place it in
`src-tauri/binaries/` with the target-triple suffix Tauri expects, for example
`convertd-x86_64-pc-windows-msvc.exe`.

Place the reviewed `llama-server` binary in `src-tauri/binaries/` using the same
target-triple convention.

### 3. Develop the app

```bash
npm ci
npm run check
npm run tauri dev
```

A production installer should be built through
`.github/workflows/windows-package.yml`. The workflow requires SHA-256 values
for both the llama-server executable and reviewed model-bundle ZIP, verifies
every locked model file, and bundles the model resources into the NSIS
installer.

### 4. Configure BackLog

The Settings tab exposes:

- Processing folder: the OneDrive-synced folder Flow 1 fills;
- Outbox folder: manifests land in `<outbox>/_manifests`;
- Quarantine folder: local, not synced;
- Cache folder: local, not synced;
- Qwen3 primary and escalation GGUF paths, resolved automatically from bundled
  resources in an installed Windows build; and
- optional fine-tuned Ettin model directory.

Run preflight before Start. Preflight checks the folders, both executables,
model files, grammar resource, and a bounded live ping to the conversion
sidecar.

### 5. Build the Power Automate flows

Build the two flows per `power-automate/FLOW1-intake.md` and
`power-automate/FLOW2-commit.md`, including the `DocumentIndex`, `NeedsReview`,
and `_pa_errors` lists. For the initial multi-thousand backfill, skip Flow 1
and drop files into Processing directly in controlled batches.

Flow 2 uses `ManifestId` as its idempotency key and keeps `Sha256` as the true
content identity. Paste `power-automate/manifest.parse-json.schema.json` into
the Power Automate Parse JSON action. Do not paste the stricter
`power-automate/manifest.schema.json`; that file is the CI and source contract.
Three valid examples live in `power-automate/examples/`.

Validate both schemas and every example before editing the flow contract:

```bash
python -m pip install -r power-automate/requirements-dev.txt
python power-automate/validate_examples.py
```

## Ettin bootstrap

1. Run a real or shadow batch of 2,000 to 5,000 files through the app.
2. Build silver labels from the ledger and cached markdown:

   ```bash
   cd training
   python silver_label.py --ledger "<AppData>/ai.sonomos.backlog/ledger.db" \
       --cache "<AppData>/ai.sonomos.backlog/cache" --out data/
   ```

3. Fine-tune:

   ```bash
   python -m pip install transformers datasets torch
   python train_ettin.py --data data/ --out ettin-backlog-v1
   ```

4. DATE F1 must be at least 0.90. PARTY and SUBJECT each require at least 0.75
   or the first release remains date-only for the Ettin lane.
5. Select the trained directory in Settings and restart BackLog.

## Behavior guarantees

- A source document is never silently deleted. Failures write a manifest before
  the local copy is moved to quarantine.
- No date ships unless it appears in document evidence or permitted metadata.
- Undated documents fall back to file modified date with
  `date_source: metadata`.
- Every physical delivery has a stable `ManifestId`. Byte-identical files share
  one true SHA-256 but receive distinct reserved filenames and index rows.
- Replaying the same physical instance reuses its ManifestId and reservation.
- A paused watcher does not consume the file's only event.
- A timed-out sidecar is killed and lazily restarted, and a timeout becomes
  terminal only after a durable flagged manifest exists.
- Crash recovery uses the ledger, stable delivery path, and existing manifests
  to resume or safely replay unfinished work.

## Repo map

```
src/                    frontend (TypeScript, Vite)
src-tauri/src/          Rust trust core and orchestrator
  pipeline.rs             content pipeline and retry ladder
  recovery.rs             pause and wall-clock recovery boundary
  checker.rs              deterministic final authority
  harvest.rs              deterministic evidence harvest
  filter.rs               evidence bundle assembly
  identity.rs             stable physical-delivery identity
  slm.rs                  Qwen llama-server lifecycle and JSON Schema decoding
  sidecar.rs              bounded convertd protocol client
  ledger.rs               jobs, file instances, and name reservations
  watcher.rs              sync-stability and durable delivery assignment
  manifest.rs             schema v2 validation and atomic handoff
sidecar/                convertd and Windows freeze instructions
models/                 Qwen/model staging, lock verification, grammar fallback
training/               Ettin silver labeling and fine-tuning
power-automate/         flows, schemas, examples, and validator
```

## Validation

Run all checks available on the current machine:

```bash
npm ci
npm run check
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --all-targets
python -m unittest discover -s sidecar/tests -v
python -m unittest discover -s models/tests -v
python -m compileall -q sidecar training models power-automate scripts
python power-automate/validate_examples.py
```

A merge does not by itself authorize production deployment. Complete
`docs/RELEASE_CHECKLIST.md` and the staged pilot in `docs/PILOT_RUNBOOK.md`
with the exact installer, model lock, llama-server hash, configuration, and
Power Automate versions intended for use.
