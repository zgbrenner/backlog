# BackLog

Local-first document naming and indexing pipeline. Watches a folder, converts
anything (Word, PowerPoint, PDF, scans) to markdown, has a small local language
model propose `YYYY-MM-DD Subject` plus a one-sentence description, and
validates every proposal with a deterministic checker. Choose the default
**Power Automate / SharePoint** handoff or **Local folder** delivery: the first
writes a JSON handoff manifest for Flow 2; the second writes finished renamed
documents directly to a local folder with a receipt per delivery.

No cloud inference. Every model runs on-device. The only outbound requests the
app ever makes are an optional model download and a startup update check; see
[`docs/PRIVACY.md`](docs/PRIVACY.md) for the exact list.

**If you are the person who runs this, not the person who builds it, read
[`docs/USER_GUIDE.md`](docs/USER_GUIDE.md) instead of this file.**

| | |
|---|---|
| Running it | [`docs/USER_GUIDE.md`](docs/USER_GUIDE.md) |
| No-installer Windows package | [`docs/PORTABLE.md`](docs/PORTABLE.md) |
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
      llama-server sidecar: bundled Qwen3-0.6B primary,
                optional Qwen3-1.7B escalation,
                JSON-schema-constrained chat completions
                                      |
                       deterministic checker (Rust)
                                      |
                 Power Automate / SharePoint (default)       Local folder
                 manifests -> Outbox/_manifests (synced)     renamed document -> Local Output
                 Flow 2: rename, archive, index list row    receipt -> Local Output/.backlog/receipts
                 (flagged files wait in local Quarantine)
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

### 0. Git hooks — do this before you write any code

```
pwsh scripts/install-hooks.ps1        # or: bash scripts/install-hooks.sh
```

Points this clone's `core.hooksPath` at the tracked `.githooks/` directory. It
is one command and is idempotent. GitHub Actions also runs the same gates on
this public repository at no billable-minute cost. Its workspace job uses the
marker fixture from `scripts/dev-stubs.sh`, so Tauri's model-resource glob is
satisfied without putting a large model in source control.

- **pre-commit** — `cargo fmt --check` plus the five gates that only read files
  (versions agree, troubleshooting coverage, button labels, CI parity, dev-stub
  marker). About two seconds, deliberately: a pre-commit hook that takes minutes
  gets bypassed with `--no-verify` and then guards nothing.
- **pre-push** — the whole of `./scripts/ci-local.sh`, all five jobs, about three
  minutes. This predicts hosted CI and is the last point at which a
  broken commit is still cheap. It skips when every ref you are pushing is a
  commit `origin` already has — `git push origin v1.2.3` straight after pushing
  the branch is the same tree the gates just passed, and running them twice both
  wastes three minutes and lets two cargo builds fight over `target/`.

Both are skippable when you mean it — `BACKLOG_SKIP_HOOKS=1 git push`, or
`git push --no-verify` — and each prints how when it fails. `.githooks/` is
tracked, which is the whole reason for `core.hooksPath`: a symlink into
`.git/hooks/` dies with the clone that made it, so the next clone would silently
have no enforcement at all.

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
the guarded GitHub release workflow runs `scripts/verify-binaries.ps1`, which
refuses to package them. A direct local `npm run tauri build` does not run that
release gate, so use it only for development unless you have staged and
verified the real release inputs described in `RELEASING.md`. The deterministic
trust core needs none of this:
`cargo test -p backlog-core` works on a bare checkout.

```
npm run tauri dev      # development
npm run tauri build    # local Windows package; not authorized for publication
```

### 2. Models

The Windows installer contains the verified Qwen3 0.6B primary model. On first
launch BackLog moves it into the per-user app-data model directory, so the
first document can be named offline without a second download.

The escalation tier — a third naming attempt on a document that failed two — is
chosen from installed RAM:

| Installed RAM | primary | escalation |
|---|---|---|
| <= 9 GiB, or unknown | Qwen3-0.6B-Q8_0 (bundled) | collapsed onto the primary |
| more than 9 GiB | Qwen3-0.6B-Q8_0 (bundled) | Qwen3-1.7B-Q8_0 |

The 1.7B is an optional in-app download of about 1.8 GB: Settings,
**Download models**. It is not in the installer because carrying it would take
the installer and the portable ZIP over GitHub's 2 GiB per-release-asset limit.
The transfer can be cancelled and resumed, and BackLog SHA-256-verifies each
file before use. Without it BackLog is fully functional — readiness says the
optional model is absent and the primary handles escalation attempts.

A larger 1.7B/4B pair was measured against this one and rejected: 2.0x the wall
clock for no quality difference that survived the sample. `docs/SIZING.md` has
that comparison and the memory arithmetic behind the table.

Release maintainers can reproduce or audit the model lock from a connected
staging machine:

```
cd models
pip install huggingface_hub
python download_models.py                # fetch + write models.lock.json
python download_models.py --verify-only  # re-verify, no network, no Hub client
```

Those two are the only flags. Commit `models.lock.json`; the weights stay
untracked. This is a release-maintenance path, not an installation step for the
person using BackLog.

### 3. Sidecar binary

See `sidecar/BUILD.md`. Produces a PyInstaller **onedir** tree; stage all of it
at `src-tauri/binaries/convertd/`, so `convertd.exe` sits beside its
`_internal/` folder. It ships through `bundle.resources`, not `externalBin`,
which can only carry a single file. `llama-server` *is* a single file and still
takes the target-triple suffix Tauri expects, e.g.
`llama-server-x86_64-pc-windows-msvc.exe`, staged with its DLLs —
`RELEASING.md` Build steps 1-2.

### 4. In-app configuration (Settings tab)

- Output mode: **Power Automate / SharePoint** is the visible default and
  writes a Flow 2 handoff manifest; **Local folder** writes the completed,
  renamed document itself and a receipt at
  `<local-output>/.backlog/receipts/<manifest_id>.json`.
- Processing folder: the watched intake folder; it can be OneDrive-synced when
  Flow 1 fills it, or any local intake folder in Local mode.
- Outbox folder: required only in Power Automate mode; it is OneDrive-synced
  and manifests land in `<outbox>/_manifests`.
- Local Output folder: required only in Local mode; it is where completed,
  renamed documents and their receipts land. Local mode never writes an Outbox
  manifest or SharePoint index.
- Quarantine folder: local, not synced
- GGUF paths for the bundled primary and optional escalation tier (advanced;
  the defaults are set by the installer)
- Ettin model dir: leave blank — the shipped sidecar ignores it. See
  `training/README.md`.

The three folders required by the selected mode — Processing, Quarantine, and
Outbox or Local Output — must be distinct and none may be nested inside
another. Preflight refuses otherwise, because an output below the
recursively-watched Processing folder would feed BackLog's own output back
through the pipeline.

### Local Output quick start

1. In **Settings**, choose **Local folder** under **Output mode**.
2. Choose separate **Processing**, **Local Output**, and **Quarantine**
   folders, then choose **Save and check this computer**.
3. Press **Start**, then drop documents into Processing.
4. Finished renamed documents appear in Local Output. Each delivery has a JSON
   receipt at `.backlog/receipts/<manifest_id>.json` below that folder.

BackLog never overwrites an existing output: it uses a deterministic suffix
such as `(2)` when needed. In Local mode it removes the source only after the
renamed output and its receipt are durably written. Restart/recovery replays
unfinished work safely. Flagged files stay in Quarantine; approving a
correction files it directly from Quarantine into Local Output, while a
dismissal leaves it in Quarantine for manual handling.

**What the watcher skips.** Files whose name begins with `~$` (Office lock
files) or `.` (dotfiles). Nothing else. A leading underscore used to be on that
list, which silently dropped real documents like `_DRAFT Agreement.docx` with
no ledger row, no manifest and no log line.

Cache retention: converted markdown is written to `<cache>` for the review
pane, then purged the moment a file is emitted — raw document text is not kept
on disk. Flagged files keep their cache until you resolve them. To retain the
corpus for Ettin training, set `"retain_cache": true` in `backlog.config.json`;
`cache_ttl_days` (default 7) sweeps orphaned entries on startup.

Hit Start. **Processing** is the watched intake folder: the watcher sweeps
existing files, then processes new arrivals.
BackLog runs as a system-tray appliance: closing the window hides it and the
pipeline keeps running in the background — quit from the tray menu.

### 5. Power Automate / SharePoint handoff

These are **automated cloud flows** built in the Power Automate web portal,
not desktop flows. Power Automate for desktop is not required for the BackLog
handoff described here; use the SharePoint and OneDrive for Business cloud
connectors and triggers named in the flow guides.

Start with `power-automate/BUILD-GUIDE.md`, then build the two flows per
`power-automate/FLOW1-intake.md` and
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
- **Needs Review means a person must decide.** BackLog does not silently file a
  document without a trustworthy date; it keeps the document visible for
  review.
- **Delivery is mode-aware.** In Power Automate mode, Done means BackLog wrote
  the manifest handoff; Flow 2 owns the later SharePoint copy, rename, and
  index completion. In Local mode, Done means BackLog durably wrote the
  renamed document and its `.backlog/receipts/<manifest_id>.json` receipt; it
  does not write a Power Automate manifest, SharePoint index, or cloud archive.
- **No model-proposed date ships unless it appears verbatim in the document
  text or in the document's own embedded metadata.** This is `checker.rs`'s
  `DateNotInEvidence` rule and it is the product's central promise. A human
  override in the review pane is recorded as `date_source: "human"`, so the
  index distinguishes a name a person chose from one the model proposed.

  "Embedded metadata" means the properties inside the file — a PDF
  `CreationDate`, a Word `dcterms:created` — which `convertd` reads and the
  evidence bundle shows the model. It deliberately does **not** include the
  file's own modified and created *timestamps*. Those are the fallback below,
  and counting them as evidence let the laziest possible answer through: a file
  that just arrived in the Processing folder was modified today, so a model that
  proposed today's date was "validated" against the filesystem and the real date
  on the page was lost. The model is never shown those timestamps, so a match
  was always coincidence rather than reading.
- **Undated documents fall back to the file modified date** with
  `date_source: metadata` and a `DATE_FROM_FILE_MTIME` note — honestly labeled
  in the index, never presented as if it came from the page.
- **Duplicate content keeps physical deliveries distinct.** Three byte-identical
  files at three paths produce one content SHA-256 and three stable delivery
  IDs. Names use deterministic collision suffixes (`base`, `(2)`, `(3)`) and
  no destination is overwritten. In Power Automate mode Flow 2 must use
  `manifest_id`, never `sha256`, as its replay key; in Local mode each delivery
  receives its own local receipt.
- **Restart is safe.** Files still present in Processing are re-swept from their
  last durable ledger state on the next start. A file that stops at the same
  stage across five restarts is quarantined as `CRASH_LOOP` rather than taking
  the batch down with it.
- **Idempotent recovery.** `manifest_id` is
  `SHA256(content_sha || 0 || normalized_relpath)`, stable across replay and
  distinct across deliveries. Power Automate replays the same handoff; Local
  mode reconciles the output and receipt without overwriting a file.

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
                        ci-local.sh (local mirror of the five hosted gates),
                        install-hooks (points core.hooksPath at .githooks/)
.githooks/              tracked git hooks: pre-commit (fast subset, ~2s) and
                        pre-push (all five gates)
.github/                authoritative hosted CI and guarded release workflows
```

## Tests

`./scripts/ci-local.sh` runs everything below in one pass, on Linux, with no
Windows, real sidecars, or model weights. `.githooks/pre-push` runs it for you
on every push once you have done Setup step 0; the public GitHub workflow runs
the same gates with deterministic marker resources.

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
python -m unittest discover -s sidecar/tests -t sidecar/tests
python -m unittest discover -s models/tests -t models/tests
pip install -r power-automate/requirements-dev.txt
python power-automate/validate_examples.py
node --test scripts/release-contract.test.mjs scripts/portable-contract.test.mjs scripts/validate-release-workflow.test.mjs
```

The trust core is separated out precisely so it needs none of the above: no
sidecar binaries, no icon, no app build. It covers the harvest regexes and every
checker rule (valid dates, hallucinated dates, range limits, metadata fallback,
illegal characters, generic subjects, SSN/card patterns, sentence-shape rules,
span-mismatch soft flags). Run it before touching either file; those two modules
are the product.
