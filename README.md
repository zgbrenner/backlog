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
                Lingua, GLiClass, granite, Ettin
      llama-server sidecar: Qwen3-0.6B primary, 1.7B escalation,
                JSON-schema-constrained chat completions
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

Cache retention: converted markdown is written to `<cache>` for the review
pane, then purged the moment a file is emitted — raw document text is not kept
on disk. Flagged files keep their cache until you resolve them. To retain the
corpus for Ettin training, set `"retain_cache": true` in `backlog.config.json`;
`cache_ttl_days` (default 7) sweeps orphaned entries on startup.

Hit Start. The watcher sweeps existing files, then processes on arrival.
BackLog runs as a system-tray appliance: closing the window hides it and the
pipeline keeps running in the background — quit from the tray menu.

### 5. Power Automate

Build the two flows per `power-automate/FLOW1-intake.md` and
`power-automate/FLOW2-commit.md`, including the `DocumentIndex`, `NeedsReview`,
and `_pa_errors` lists. For the initial multi-thousand backfill, skip Flow 1
and drop the files into Processing directly.

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

- A file is never deleted. Failures move to quarantine with a
  machine-readable reason and a `NeedsReview` row.
- No date ships unless it appears verbatim in the document text or file
  metadata (anti-hallucination tripwire in `checker.rs`).
- Undated documents fall back to file modified date with
  `date_source: metadata`, honestly labeled in the index.
- Duplicate content (same SHA-256) is indexed once; later copies get
  `" (2)"` names and a `duplicate_of` pointer.
- Crash mid-batch: the ledger resumes unfinished jobs on next start.
- Idempotent end-to-end: replaying any manifest cannot double-index.

## Repo map

```
src/                    frontend (vanilla TS, Vite)
src-tauri/core/         backlog-core: deterministic trust core (no Tauri, no I/O)
  checker.rs              deterministic validation (the trust core)
  harvest.rs              regex evidence harvest
src-tauri/src/          Rust app crate (Tauri shell + orchestration)
  pipeline.rs             orchestrator + retry ladder
  filter.rs               evidence bundle assembly
  slm.rs                  llama-server lifecycle + grammar decoding
  sidecar.rs              convertd protocol client
  ledger.rs               SQLite state machine
  watcher.rs              debounced folder watcher w/ sync stability
  manifest.rs             atomic manifest emission + pacing
sidecar/                convertd (Python) + build instructions
models/                 download script, lockfile, GBNF grammar copy
training/               Ettin silver labeling + fine-tune
power-automate/         Flow 1 and Flow 2 build sheets
```

## Tests

The deterministic trust core (`harvest` + `checker`) lives in its own
`backlog-core` crate with no Tauri or sidecar dependency, so it tests on a bare
checkout — no sidecar binaries, no icon, no app build:

```
cd src-tauri
cargo test -p backlog-core     # trust core only, ~10s
```

That covers the harvest regexes and every checker rule (valid dates,
hallucinated dates, range limits, metadata fallback, illegal characters,
generic subjects, SSN/card patterns, sentence-shape rules, span-mismatch soft
flags). Run it before touching either file; those two modules are the product.
Because it builds without the sidecars, you can run it locally in seconds
(useful when GitHub Actions minutes aren't available).

`cargo test --workspace` additionally builds the Tauri app crate, whose build
script needs the sidecar binaries present (externalBin + the bundled llama
DLLs). On a fresh checkout that hasn't built them yet, run `scripts/dev-stubs.ps1`
(or `scripts/dev-stubs.sh`) once to stage empty placeholders — a local
convenience, not required for iterating on the trust core, and superseded by the
real binaries at release time.
