# BackLog

Local-first document naming and indexing pipeline. Watches a folder, converts
anything (Word, PowerPoint, PDF, scans) to markdown, has a small local language
model propose `YYYY-MM-DD Subject` plus a one-sentence description, validates
every proposal with a deterministic checker, and hands Power Automate a JSON
manifest to rename, archive, and index the file in SharePoint.

No cloud inference. No terminals. Every model runs on-device; the only cloud
surfaces are SharePoint and the two Power Automate flows.

Design doc: `2026-07-21 Sortition Pipeline Design.md` (the naming predates the
app name; the architecture is current).

## Architecture at a glance

```
Intake (SharePoint) --Flow 1--> Processing folder (OneDrive-synced)
                                      |
                                  [BackLog app]
                systray Tauri shell, Rust core, SQLite ledger
                                      |
      convertd sidecar (Python): MarkItDown, pdfium, RapidOCR,
                fastText, GLiClass, granite, Ettin
      llama-server sidecar: LFM2.5-350M primary, 1.2B escalation,
                GBNF grammar-locked decoding
                                      |
                       deterministic checker (Rust)
                                      |
                     manifests -> Outbox/_manifests (synced)
                                      |
              Flow 2: rename, copy to Archive, index list row
              (flagged files -> local quarantine + NeedsReview list)
```

## Prerequisites (build machine)

- Rust stable + the Tauri 2 prerequisites for your OS
  (<https://tauri.app/start/prerequisites/>)
- Node 18+
- Python 3.11+ (for the sidecar and the model fetch)
- A `llama-server` binary from llama.cpp, **mid-2024 or newer** (the naming
  grammar uses bounded repetition `{m,n}`, added to GBNF in 2024). Grab a
  release build for your platform from the llama.cpp releases page.

## Setup

### 1. Models (on a machine with internet, once)

```
cd models
pip install huggingface_hub
python download_models.py          # core set
python download_models.py --vl     # optional: VL-Extract scan fallback
```

This writes `models.lock.json` with SHA-256 hashes recorded on first download
and verified on every later run. Commit the lockfile. Copy `models/` to the
deployment machine if it differs.

### 2. Sidecar binary

See `sidecar/BUILD.md`. Produces `convertd(.exe)`; place it in
`src-tauri/binaries/` with the target-triple suffix Tauri expects, e.g.
`convertd-x86_64-pc-windows-msvc.exe`.

Place the `llama-server` binary in `src-tauri/binaries/` the same way.

### 3. App

```
npm install
npm run tauri dev      # development
npm run tauri build    # production installer
```

### 4. In-app configuration (Settings tab)

- Processing folder: the OneDrive-synced folder Flow 1 fills
- Outbox folder: OneDrive-synced; manifests land in `<outbox>/_manifests`
- Quarantine folder: local, not synced
- GGUF paths for the two SLM tiers (defaults point into `models/`)
- Ettin model dir: blank until slice 4 (see below)

Hit Start. The watcher sweeps existing files, then processes on arrival.

### 5. Power Automate

Build the two flows per `power-automate/FLOW1-intake.md` and
`power-automate/FLOW2-commit.md`, including the `DocumentIndex`, `NeedsReview`,
and `_pa_errors` lists. For the initial multi-thousand backfill, skip Flow 1
and drop the files into Processing directly.

Flow 2 uses `ManifestId` as its idempotency key and keeps `Sha256` as the true
content identity. Paste `power-automate/manifest.parse-json.schema.json` into
the Power Automate Parse JSON action. Do not paste the stricter
`power-automate/manifest.schema.json`; that file is the CI and source contract.
Three valid examples live in `power-automate/examples/`.

Validate both schemas and every example before editing the flow contract:

```
python -m pip install -r power-automate/requirements-dev.txt
python power-automate/validate_examples.py
```

## Ettin bootstrap (slice 4)

1. Run a real batch (or shadow batch) of 2-5K files through the app.
2. Build silver labels from the ledger + cached markdown:
   ```
   cd training
   python silver_label.py --ledger "<AppData>/ai.sonomos.backlog/ledger.db" \
       --cache "<AppData>/ai.sonomos.backlog/cache" --out data/
   ```
3. Fine-tune:
   ```
   pip install transformers datasets torch
   python train_ettin.py --data data/ --out ettin-backlog-v1
   ```
4. Ship gate printed at the end: DATE F1 >= 0.90 required; PARTY/SUBJECT
   >= 0.75 each or ship date-only.
5. Point Settings -> Ettin model dir (and `BACKLOG_ETTIN_DIR` for a frozen
   sidecar) at the output directory. Restart the pipeline.

## Behavior guarantees

- A source document is never silently deleted. Failures write a manifest and
  move the local copy to quarantine with a machine-readable reason.
- No date ships unless it appears verbatim in the document text or file
  metadata (anti-hallucination tripwire in `checker.rs`).
- Undated documents fall back to file modified date with
  `date_source: metadata`, honestly labeled in the index.
- Every physical file instance has a stable `ManifestId`. Byte-identical files
  share one true SHA-256, but receive distinct reserved filenames and separate
  index rows. Later copies carry a `duplicate_of` content pointer.
- Replaying the same physical instance reuses its ManifestId and filename, so
  it cannot create a second archive file or index row.
- Crash mid-batch: the ledger and manifest handoff preserve enough state to
  resume or safely replay unfinished work.

## Repo map

```
src/                    frontend (vanilla TS, Vite)
src-tauri/src/          Rust core
  pipeline.rs             orchestrator + retry ladder
  checker.rs              deterministic validation (the trust core)
  harvest.rs              regex evidence harvest
  filter.rs               evidence bundle assembly
  identity.rs             stable physical-file instance identity
  slm.rs                  llama-server lifecycle + grammar decoding
  sidecar.rs              convertd protocol client
  ledger.rs               content jobs, file instances, name reservations
  watcher.rs              debounced folder watcher w/ sync stability
  manifest.rs             schema v2 validation + atomic handoff
sidecar/                convertd (Python) + build instructions
models/                 download script, lockfile, GBNF grammar copy
training/               Ettin silver labeling + fine-tune
power-automate/         flows, strict schema, Parse JSON schema, examples
```

## Tests

The Rust suite covers harvest regexes, deterministic checker rules, stable file
instance IDs, transactional filename reservations, duplicate-three-file
behavior, manifest replay, path safety, and guarded human-review transitions.
The Python contract validator checks every manifest example against both the
strict source schema and the Power Automate-compatible Parse JSON schema.

Run the available checks before changing the trust core or handoff contract:

```
npm run check
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo test --manifest-path src-tauri/Cargo.toml --all-targets
python -m unittest discover -s sidecar/tests -v
python -m compileall -q sidecar training models power-automate
python power-automate/validate_examples.py
```
