# BackLog

Local-first document naming and indexing pipeline. Watches a folder, converts
anything (Word, PowerPoint, PDF, scans) to markdown, has a small local language
model propose `YYYY-MM-DD Subject` plus a one-sentence description, validates
every proposal with a deterministic checker, and hands Power Automate a JSON
manifest to rename, archive, and index the file in SharePoint.

No cloud inference. Every model runs on-device. The only outbound requests the
app ever makes are a one-time model download and a startup update check; see
[`docs/PRIVACY.md`](docs/PRIVACY.md) for the exact list.

**If you are the person who runs this, not the person who builds it, read
[`docs/USER_GUIDE.md`](docs/USER_GUIDE.md) instead of this file.**

| | |
|---|---|
| Running it | [`docs/USER_GUIDE.md`](docs/USER_GUIDE.md) |
| Every message it can show you | [`docs/TROUBLESHOOTING.md`](docs/TROUBLESHOOTING.md) |
| What it does with your documents | [`docs/PRIVACY.md`](docs/PRIVACY.md) · [`docs/SECURITY.md`](docs/SECURITY.md) |
| Why it is built this way | [`docs/DECISIONS.md`](docs/DECISIONS.md) |
| What it needs from the machine, measured | [`docs/SIZING.md`](docs/SIZING.md) |
| What is not finished | [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md) |
| Cutting a release | [`RELEASING.md`](RELEASING.md) · [`docs/RELEASE_CHECKLIST.md`](docs/RELEASE_CHECKLIST.md) |
| Third-party components | [`NOTICE.md`](NOTICE.md) · [`LICENSE`](LICENSE) |

`2026-07-21 Sortition Pipeline Design.md` is **historical design rationale,
superseded on models, error codes and Flow 2 behavior**. Do not implement
against it; read the code.

## Architecture at a glance

```
Intake (SharePoint) --Flow 1--> Processing folder (OneDrive-synced)
                                      |
                                  [BackLog app]
                systray Tauri shell, Rust core, SQLCipher ledger
                                      |
      convertd sidecar (Python, slim/torch-free): MarkItDown, pdfium,
                RapidOCR, Lingua (GLiClass/granite/Ettin naming enhancements
                degrade to deterministic fallbacks; not shipped by default)
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

- **Rust** — the channel in `rust-toolchain.toml`, plus the [Tauri 2
  prerequisites](https://tauri.app/start/prerequisites/) for your OS. On Linux
  that is `libwebkit2gtk-4.1-dev libjavascriptcoregtk-4.1-dev libsoup-3.0-dev
  libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev patchelf`; see
  `.github/workflows/ci.yml`, which lists exactly that set.
- **Node 22** (the version the gates and the release build use).
- **Python 3.11, exactly.** Not "3.11 or newer":
  `scripts/build-sidecar.ps1` hard-throws on anything else, because onnxruntime
  and rapidocr publish no 3.13/3.14 wheels.
- **A `llama-server` binary from llama.cpp.** The build verified for this pilot
  is release **`b10091`** (`llama-b10091-bin-win-cpu-x64.zip`, SHA-256 in
  `RELEASING.md` Build step 2). The real requirement is not a date — it is that the
  server supports `response_format: {"type": "json_schema", …}` **and**
  `chat_template_kwargs` on `/v1/chat/completions`, which is how `slm.rs`
  constrains the model's output and disables Qwen3's thinking mode. An older
  build accepts both keys and silently ignores them: the model then emits free
  text, the checker rejects every proposal, and every document ends in
  `SLM_FAIL` with a reason that points at the model rather than at the binary.
  Note that `llama-server.exe` is a thin stub that loads about 13 runtime DLLs
  (`llama*.dll`, `ggml*.dll`, `mtmd.dll`, `libomp140.x86_64.dll`); they must be
  staged beside it — see `RELEASING.md` Build step 2.

## Setup

### 1. App

```
npm install
```

On a **fresh clone**, `src-tauri/binaries/` is empty (it is gitignored) and
`tauri-build` refuses to compile the app crate without it. From the repo root,
stage placeholders once:

```
bash scripts/dev-stubs.sh        # or: pwsh scripts/dev-stubs.ps1
```

These are marked `BACKLOG-DEV-STUB-DO-NOT-SHIP` and
`scripts/verify-binaries.ps1` refuses to package them, so they cannot reach a
release by accident. The deterministic trust core needs none of this:
`cargo test -p backlog-core` works on a bare checkout.

```
npm run tauri dev      # development
npm run tauri build    # production installer (Windows)
```

### 2. Models

**Normally there is nothing to do here.** The app downloads the two Qwen3 GGUFs
itself: Settings → Readiness → **Download models (~2.4 GB)**. The download is
resumable, cancellable, and SHA-256-verified against `models.lock.json`. That
button exists so the person running this never opens a terminal.

The files land in `%APPDATA%\ai.sonomos.backlog\models`, which is also where
the app rehomes the default model paths at startup — they are **not** in the
installer (`tauri.conf.json`'s `bundle.resources` maps only `resources/*` and
`binaries/*.dll`). A path you set yourself through Settings → Browse is honored
untouched.

To stage them from a connected machine instead (air-gapped deployment, or
producing the lockfile for a release):

```
cd models
pip install huggingface_hub
python download_models.py                # fetch + write models.lock.json
python download_models.py --verify-only  # re-verify, no network, no Hub client
```

Those two are the only flags. Commit `models.lock.json`; the weights stay
untracked. Copy the two `.gguf` files into
`%APPDATA%\ai.sonomos.backlog\models` on the deployment machine, or point
Settings at wherever you put them.

### 3. Sidecar binary

See `sidecar/BUILD.md`. Produces `convertd(.exe)`; place it in
`src-tauri/binaries/` with the target-triple suffix Tauri expects, e.g.
`convertd-x86_64-pc-windows-msvc.exe`. Place `llama-server` (and its DLLs) the
same way — `RELEASING.md` Build steps 1-2.

### 4. In-app configuration (Settings tab)

- Processing folder: the OneDrive-synced folder Flow 1 fills
- Outbox folder: OneDrive-synced; manifests land in `<outbox>/_manifests`
- Quarantine folder: local, not synced
- GGUF paths for the two SLM tiers (default to the app-data models dir)
- Ettin model dir: leave blank — the shipped sidecar ignores it. See
  `training/README.md`.

All three folders must be distinct and none may be nested inside another;
preflight refuses otherwise, because an Outbox under the recursively-watched
Processing folder feeds the app's own manifests back through the pipeline.

**What the watcher skips.** Files whose name begins with `~$` (Office lock
files) or `.` (dotfiles). Nothing else. A leading underscore used to be on that
list, which silently dropped real documents like `_DRAFT Agreement.docx` with
no ledger row, no manifest and no log line.

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
and `_pa_errors` lists.

**For the initial multi-thousand-file backfill**, skip Flow 1 and drop the
files into Processing directly, in batches — and set
`"manifest_emit_per_min": 10` in `backlog.config.json` first. The shipped
default is `0`, meaning unlimited, which hands SharePoint manifests as fast as
the pipeline produces them and is the documented route to the connector `429`s
that `docs/PILOT_RUNBOOK.md` Stage 2 gates on.

## Behavior guarantees

These are the claims the product actually makes. Each one is enforced by code
you can point at, and by a test.

- **A failed file is moved, not deleted.** Anything the pipeline refuses lands
  in your Quarantine folder (a rename, or a copy-then-remove when Quarantine is
  on another volume) with a machine-readable reason and a `NeedsReview` row. If
  the move itself fails, that is surfaced as `QUARANTINE_FAILED` and the source
  is left where it is rather than orphaned.
- **No model-proposed date ships unless it appears verbatim in the document
  text or in the file's metadata.** This is `checker.rs`'s
  `DateNotInEvidence` rule and it is the product's central promise. A human
  override in the review pane is recorded as `date_source: "human"`, so the
  index distinguishes a name a person chose from one the model proposed.
- **Undated documents fall back to the file modified date** with
  `date_source: metadata` and a `DATE_FROM_FILE_MTIME` note — honestly labeled
  in the index, never presented as if it came from the page.
- **Duplicate content is archived and indexed once per physical copy.** Three
  byte-identical files at three paths produce one content SHA-256, three
  `manifest_id`s, three index rows and three names (`base`, `(2)`, `(3)`), with
  `duplicate_of` pointing at the shared hash. This is deliberate and
  `FLOW2-commit.md` §7 tests for it — **do not** add a SHA-256 idempotency gate
  to Flow 2; it would silently drop every second copy of a duplicated document.
  Flow 2's replay key is `manifest_id`, never `sha256`.
- **Restart is safe.** Files still present in Processing are re-swept from their
  last durable ledger state on the next start. A file that stops at the same
  stage across five restarts is quarantined as `CRASH_LOOP` rather than taking
  the batch down with it.
- **Idempotent end-to-end.** Replaying any manifest cannot double-index:
  `manifest_id` is `SHA256(content_sha || 0 || normalized_relpath)`, stable
  across replays and distinct across deliveries.

## Repo map

```
src/                    frontend (vanilla TS, Vite)
src-tauri/core/         backlog-core: deterministic trust core (no Tauri, no I/O)
  checker.rs              deterministic validation (the trust core)
  harvest.rs              regex evidence harvest
src-tauri/src/          Rust app crate (Tauri shell + orchestration)
  pipeline.rs             orchestrator + retry ladder
  preflight.rs            live readiness checks + plain-language problems
  filter.rs               evidence bundle assembly
  slm.rs                  llama-server lifecycle + JSON-schema decoding
  sidecar.rs              convertd protocol client
  ledger.rs               SQLCipher state machine
  dbkey.rs                DPAPI-protected ledger key
  model_download.rs       resumable, hash-verified in-app model fetch
  manifest.rs             atomic manifest emission (v3) + pacing
  watcher.rs              debounced folder watcher w/ sync stability
  logging.rs              structured logging with path redaction
sidecar/                convertd (Python) + build instructions
models/                 download script, lockfile, GBNF grammar copy
training/               Ettin silver labeling + fine-tune (not shipped)
power-automate/         Flow 1 and Flow 2 build sheets + manifest schemas
scripts/                sidecar build, dev stubs, release binary verification,
                        ci-local.sh (the five gates, and what actually runs them)
.github/                the same five jobs as a workflow, for the day Actions works
```

## Tests

`./scripts/ci-local.sh` runs everything below in one pass, on Linux, with no
Windows, no sidecar binaries and no model weights. Run it before you push;
`.github/workflows/ci.yml` describes the same five jobs but has never been
assigned a runner, so nothing runs them for you (`docs/KNOWN_ISSUES.md` item
11).

```
cd src-tauri
cargo test -p backlog-core                     # trust core only, ~10s, bare checkout
cargo test --workspace --all-targets           # + the app crate (needs dev-stubs first)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

```
npm run check           # tsc --noEmit + vite build
npm run harness:shots   # renders the real UI in headless Chromium against a
                        # mock Tauri IPC; exits non-zero on any console error,
                        # so it is a smoke test, not just a screenshotter
python -m pytest sidecar/tests models/tests
pip install -r power-automate/requirements-dev.txt
python power-automate/validate_examples.py
```

The trust core is separated out precisely so it needs none of the above: no
sidecar binaries, no icon, no app build. It covers the harvest regexes and every
checker rule (valid dates, hallucinated dates, range limits, metadata fallback,
illegal characters, generic subjects, SSN/card patterns, sentence-shape rules,
span-mismatch soft flags). Run it before touching either file; those two modules
are the product.
