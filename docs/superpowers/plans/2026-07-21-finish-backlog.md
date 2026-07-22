# BackLog Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the current BackLog prototype into a testable, failure-safe pilot build that can process real document batches without silent skips, duplicate-name corruption, Unicode crashes, or ambiguous Power Automate commits.

**Architecture:** Preserve the approved Tauri 2 desktop architecture, Rust trust core, local Python conversion sidecar, local llama.cpp inference lane, SQLite ledger, and manifest-driven Power Automate handoff. Strengthen the boundaries between a content hash, a physical file instance, a reserved filename, and an emitted manifest so retries and duplicate content remain deterministic. Add preflight checks, time-bounded sidecar I/O, durable pause behavior, CI, fixture-driven tests, schemas, and operational documentation.

**Tech Stack:** Tauri 2, Rust 2021, Tokio, rusqlite, TypeScript, Vite, Python 3.11+, PyInstaller, llama.cpp, GitHub Actions, Power Automate, SharePoint.

## Global Constraints

- Runtime inference, conversion, OCR, classification, and validation remain fully local. No cloud inference or content telemetry.
- Power Automate performs controlled I/O only. It must not become the naming or deterministic-validation engine.
- The app never silently overwrites or deletes a source document.
- A content SHA-256 remains the actual content hash. It must never be overloaded as a per-file or per-manifest identifier.
- Every physical file instance receives a stable instance identifier so replay is idempotent and duplicate content can still be handled separately.
- The deterministic checker remains the final authority before an `ok` manifest is emitted.
- File and manifest names must be valid on Windows and SharePoint.
- The pipeline must handle non-ASCII and multilingual text without byte-boundary panics.
- A paused watcher must not lose files that arrive while paused.
- A hung sidecar request must time out, terminate the unusable sidecar process, and leave the file recoverable.
- No production-ready claim is made without fresh frontend, Rust, Python, schema, and packaging validation evidence.

---

## File Map

- `.github/workflows/ci.yml`: parallel frontend, Rust, Python, schema, and documentation validation.
- `.github/workflows/windows-package.yml`: manual Windows packaging smoke test with explicit external-binary/model prerequisites.
- `src-tauri/src/identity.rs`: stable physical-file instance identifiers and safe manifest filenames.
- `src-tauri/src/ledger.rs`: file-instance tracking and transactional filename reservations.
- `src-tauri/src/manifest.rs`: manifest schema v2, atomic writes, and filename selection by manifest ID.
- `src-tauri/src/pipeline.rs`: pause-safe dispatch, instance-aware idempotency, timeout handling, and terminal-state emission.
- `src-tauri/src/sidecar.rs`: time-bounded request/response transport and sidecar restart behavior.
- `src-tauri/src/config.rs`: preflight validation and path-overlap protection.
- `src-tauri/src/harvest.rs`: Unicode-safe head/tail slicing.
- `src-tauri/src/checker.rs`: assertion repair and expanded trust-core tests.
- `src-tauri/src/lib.rs`: runtime-status and preflight commands for the UI.
- `src/main.ts`, `src/styles.css`: first-run preflight, accurate runtime state, actionable errors, and safer review controls.
- `sidecar/tests/`: lightweight unit tests that do not download models.
- `power-automate/manifest.schema.json`: authoritative Flow 2 parse schema.
- `power-automate/examples/`: valid `ok`, duplicate, and `flagged` manifests.
- `power-automate/FLOW2-commit.md`: ManifestId-based idempotency and content-hash duplicate handling.
- `docs/PILOT_RUNBOOK.md`: setup, dry run, pilot gates, rollback, and evidence collection.
- `docs/RELEASE_CHECKLIST.md`: Windows installer and offline-runtime release gates.

---

### Task 1: Establish Continuous Validation

**Files:**
- Create: `.github/workflows/ci.yml`
- Create: `sidecar/tests/test_convertd_unit.py`
- Modify: `package.json`

**Interfaces:**
- Consumes: existing frontend build, Rust crate, and import-light Python sidecar module.
- Produces: repeatable CI commands used by every later task.

- [ ] **Step 1: Add lightweight Python tests before loading heavyweight models**

Test `_page_indices`, `_normalize_span_date`, `_letterhead_resets`, protocol unknown-op errors, and lockfile helper behavior using `unittest`. Importing `sidecar/convertd.py` must not initialize any model.

- [ ] **Step 2: Add frontend validation scripts**

Add `typecheck` as `tsc --noEmit`, retain `build`, and add `check` as `npm run typecheck && vite build`.

- [ ] **Step 3: Add parallel CI jobs**

Run:

```text
npm install --ignore-scripts
npm run check
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo test --manifest-path src-tauri/Cargo.toml --all-targets
python -m unittest discover -s sidecar/tests -v
python -m compileall -q sidecar training models
python -m json.tool power-automate/manifest.schema.json
```

Install the documented Ubuntu packages required for Tauri compilation. Cache Cargo and npm downloads without caching model weights.

- [ ] **Step 4: Run each command independently and record any baseline failures**

Expected: frontend build passes; Rust compile/tests either pass or expose concrete source defects; Python unit/compile checks pass; no model download occurs.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml sidecar/tests package.json
git commit -m "ci: validate frontend rust and python"
```

---

### Task 2: Repair Trust-Core Test Integrity and Unicode Safety

**Files:**
- Modify: `src-tauri/src/checker.rs`
- Modify: `src-tauri/src/harvest.rs`

**Interfaces:**
- Consumes: converted Markdown as UTF-8 text.
- Produces: panic-free `Harvest` values and checker tests that genuinely fail on regressions.

- [ ] **Step 1: Write failing tests**

Add tests that harvest text containing Danish, Spanish, and composed Unicode characters where the 6,000-byte and 2,500-byte boundaries fall inside a multi-byte scalar. Add a checker test that explicitly asserts `CheckError::DateNotInEvidence`.

- [ ] **Step 2: Verify the tests fail against the current implementation**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml harvest::tests::unicode_boundaries checker::tests::rejects_hallucinated_date
```

Expected: the Unicode test panics or fails, and the checker test reveals the non-asserting `matches!` expression.

- [ ] **Step 3: Add UTF-8 boundary helpers**

Use helpers that move a requested byte index down to the nearest `is_char_boundary` for prefixes and up/down safely for suffixes. Replace direct `&markdown[..head_len]` and `&markdown[tail_start..]` slicing.

- [ ] **Step 4: Replace the ignored `matches!` value with an assertion**

Use `assert!(matches!(e, CheckError::DateNotInEvidence(_)));` and scan all tests for other unused boolean assertions.

- [ ] **Step 5: Run the full trust-core suite**

```bash
cargo test --manifest-path src-tauri/Cargo.toml checker harvest
```

Expected: all trust-core tests pass with no panic.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/checker.rs src-tauri/src/harvest.rs
git commit -m "fix: make trust core unicode safe"
```

---

### Task 3: Separate Content Identity, File Instances, and Manifest Identity

**Files:**
- Create: `src-tauri/src/identity.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/ledger.rs`
- Modify: `src-tauri/src/manifest.rs`
- Modify: `src-tauri/src/pipeline.rs`
- Modify: `src-tauri/Cargo.toml`

**Interfaces:**
- Produces: `instance_id(content_sha256: &str, normalized_relpath: &str) -> String`.
- Produces: `Ledger::register_instance`, `Ledger::instance`, `Ledger::set_instance_state`, and `Ledger::reserve_filename`.
- Produces: manifest schema version 2 with both `manifest_id` and `sha256`.

- [ ] **Step 1: Write failing identity and ledger tests**

Cover these cases:

1. Same content and same normalized path produce the same instance ID.
2. Same content at two different paths produces two instance IDs.
3. Instance IDs contain only lowercase ASCII hex and hyphens.
4. Replaying an instance returns its existing reserved filename.
5. Three duplicate-content instances reserve `name.ext`, `name (2).ext`, and `name (3).ext` without collision.
6. Concurrent reservation attempts cannot receive the same filename.

Use `tempfile` as a dev dependency for isolated SQLite tests.

- [ ] **Step 2: Add schema migrations**

Create:

```sql
CREATE TABLE IF NOT EXISTS file_instances (
  instance_id TEXT PRIMARY KEY,
  sha256 TEXT NOT NULL,
  original_path TEXT NOT NULL,
  original_name TEXT NOT NULL,
  ext TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'discovered',
  final_filename TEXT,
  manifest_id TEXT,
  created_at TEXT NOT NULL DEFAULT (...),
  updated_at TEXT NOT NULL DEFAULT (...)
);
CREATE INDEX IF NOT EXISTS idx_instances_sha ON file_instances(sha256);
CREATE TABLE IF NOT EXISTS filename_reservations (
  final_filename TEXT PRIMARY KEY,
  instance_id TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL DEFAULT (...)
);
```

- [ ] **Step 3: Implement stable IDs**

Derive the instance ID from the content SHA-256 plus a normalized relative path using SHA-256 again. Do not include a random UUID. This makes watcher replay stable while allowing identical content at different paths.

- [ ] **Step 4: Make filename reservation transactional**

Within one SQLite transaction, return an existing reservation for the same instance or insert the first free candidate. Check both legacy `jobs.final_filename` values and the reservation table during migration compatibility.

- [ ] **Step 5: Upgrade the manifest**

Add `manifest_id`, keep `sha256` as the true content hash, set schema to 2, and write `<manifest_id>.json`. Validate the manifest ID before using it as a filename.

- [ ] **Step 6: Update the pipeline**

Register the physical instance before content-level deduplication. Skip only when that exact instance is already terminal. For a new instance whose content job is already emitted, reuse the accepted metadata, reserve a distinct filename, emit a duplicate manifest with the same content SHA-256, and set `duplicate_of` to the original content SHA-256.

- [ ] **Step 7: Run identity, ledger, manifest, and pipeline tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml identity ledger manifest pipeline
```

Expected: stable replay and three-way duplicate tests pass.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/src
git commit -m "fix: make duplicate processing instance safe"
```

---

### Task 4: Make Pause, Sidecar I/O, and Terminal Failures Recoverable

**Files:**
- Modify: `src-tauri/src/config.rs`
- Modify: `src-tauri/src/sidecar.rs`
- Modify: `src-tauri/src/pipeline.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/watcher.rs`

**Interfaces:**
- Produces: `Config::validate() -> Result<(), ConfigError>`.
- Produces: `Sidecar::ping()` and time-bounded `Sidecar::call()`.
- Produces: `Pipeline::set_paused(bool)` with durable wakeup behavior.

- [ ] **Step 1: Write failing tests**

Cover overlapping Processing/Quarantine paths, zero worker counts, missing GGUF paths, sidecar timeout, stable pause/resume delivery, and manifest-write failure that must not move a source file into an unrecoverable terminal state.

- [ ] **Step 2: Validate configuration before saving or starting**

Reject empty required paths, overlapping watched/quarantine/outbox roots, out-of-range ports, zero worker/parallel counts, and missing model files. Return all validation failures in one structured response where practical.

- [ ] **Step 3: Replace blocking sidecar reads with a reader thread and `recv_timeout`**

A dedicated stdout reader thread sends complete lines over a channel. `call()` waits no longer than `Sidecar::timeout`, kills the child on timeout or closed stream, clears the process slot, and returns a typed error. The next request lazily respawns.

- [ ] **Step 4: Make pause durable**

Use a Tokio watch channel or equivalent. Jobs discovered while paused wait for the resumed value instead of returning and losing the event.

- [ ] **Step 5: Enforce the configured wall-clock cap**

Use `per_file_wall_clock_secs` directly. On timeout, terminate unusable local subprocesses, log a machine-readable timeout event, and ensure the instance is recoverable or receives a `RUNTIME_TIMEOUT` flagged manifest.

- [ ] **Step 6: Make flagged-manifest emission failure-safe**

Do not mark an instance terminal or move its source until the flagged manifest is written successfully. If the manifest cannot be written, leave the source in Processing and the instance resumable.

- [ ] **Step 7: Run reliability tests and full Rust suite**

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src
git commit -m "fix: make runtime failures recoverable"
```

---

### Task 5: Add Accurate Preflight and Operational UX

**Files:**
- Modify: `src-tauri/src/lib.rs`
- Modify: `src/main.ts`
- Modify: `src/styles.css`

**Interfaces:**
- Produces: `get_runtime_status`, `run_preflight`, and structured validation results.
- Consumes: backend state rather than frontend-only `running` and `paused` booleans.

- [ ] **Step 1: Add backend runtime-status commands**

Return `configured`, `running`, `paused`, `sidecar_ok`, `primary_model_found`, `escalation_model_found`, and an array of actionable problems.

- [ ] **Step 2: Add a first-run checklist**

Settings shows folder readiness, sidecar availability, model availability, outbox writability, and the offline-runtime guarantee. Start remains disabled until hard prerequisites pass.

- [ ] **Step 3: Make runtime state authoritative**

Refresh status from Rust after reload and after Start/Pause/Resume. Do not display a local-only state that can disagree with the pipeline.

- [ ] **Step 4: Improve review safety**

Disable resubmit while a request is pending, show inline checker violations, preserve values after failure, add accessible labels, and require a non-empty date/subject/description before invoking Rust.

- [ ] **Step 5: Run frontend check and manual DOM smoke test**

```bash
npm run check
```

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src/main.ts src/styles.css
git commit -m "feat: add preflight and reliable runtime status"
```

---

### Task 6: Make the Power Automate Contract Executable

**Files:**
- Create: `power-automate/manifest.schema.json`
- Create: `power-automate/examples/manifest-ok.json`
- Create: `power-automate/examples/manifest-duplicate.json`
- Create: `power-automate/examples/manifest-flagged.json`
- Modify: `power-automate/FLOW2-commit.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: manifest schema version 2.
- Produces: exact Parse JSON schema and list-column contract.

- [ ] **Step 1: Write JSON Schema and examples**

Require `schema`, `manifest_id`, `sha256`, `status`, `original_name`, `original_relpath`, `model_versions`, and `processed_at`. Use conditional requirements so `ok` requires naming fields and `flagged` requires `flag_reason`.

- [ ] **Step 2: Change Flow 2 idempotency**

Add indexed `ManifestId` to `DocumentIndex` and `NeedsReview`. Query by `ManifestId`, not `Sha256`. Keep `Sha256` indexed for duplicate-content reporting and analysis.

- [ ] **Step 3: Document duplicate semantics**

A duplicate-content file gets a unique `ManifestId`, the same true `Sha256`, a distinct reserved filename, and `duplicate_of` equal to the original content SHA-256. Replaying the same physical instance reuses its ManifestId and cannot double-index.

- [ ] **Step 4: Validate examples against the schema**

Use a CI JSON Schema validator and `python -m json.tool` for syntax.

- [ ] **Step 5: Commit**

```bash
git add power-automate README.md
git commit -m "docs: define manifest v2 Power Automate contract"
```

---

### Task 7: Harden Model Download and Sidecar Packaging

**Files:**
- Modify: `models/download_models.py`
- Modify: `sidecar/BUILD.md`
- Create: `scripts/build-sidecar.ps1`
- Create: `docs/RELEASE_CHECKLIST.md`
- Create: `.github/workflows/windows-package.yml`

**Interfaces:**
- Produces: deterministic lock verification, missing-file detection, and an explicit Windows packaging smoke test.

- [ ] **Step 1: Add lockfile unit tests**

Test valid files, modified files, missing locked files, untracked downloaded files, and ignored transient Hugging Face cache paths.

- [ ] **Step 2: Separate download from verification**

Add `--verify-only` and `--repair`. A normal subsequent run verifies every locked path and fails if any locked file is missing or changed. Exclude transient `.cache` and temporary files from the lock.

- [ ] **Step 3: Add a reproducible PowerShell build script**

Create the venv, install pinned build inputs, build `convertd`, copy it with the required target-triple filename, smoke-test the NDJSON `ping` operation, and fail if the output binary is missing.

- [ ] **Step 4: Add a manual Windows packaging workflow**

Validate source, build the sidecar, stage a caller-supplied llama-server binary and model paths, run `tauri build`, and upload the installer plus checksums. Do not download or publish model weights implicitly.

- [ ] **Step 5: Document release gates**

Require Windows install/uninstall, offline launch, no unexpected outbound connections, sidecar timeout recovery, duplicate-three-file fixture, Unicode fixture, scan fixture, manifest replay, and rollback verification.

- [ ] **Step 6: Commit**

```bash
git add models sidecar scripts docs .github/workflows/windows-package.yml
git commit -m "build: harden offline Windows packaging"
```

---

### Task 8: Add Pilot Fixtures, Runbook, and End-to-End Dry Run

**Files:**
- Create: `fixtures/README.md`
- Create: `fixtures/text/`
- Create: `docs/PILOT_RUNBOOK.md`
- Modify: `README.md`

**Interfaces:**
- Produces: a content-safe synthetic fixture set and measurable pilot acceptance gates.

- [ ] **Step 1: Add synthetic documents**

Include dated and undated letters, Danish text, ambiguous numeric dates, repeated duplicate content under three names, invalid/zero-byte input, a multi-document-like packet, and descriptions containing identifier-like patterns. Do not commit real Vistage documents or personal data.

- [ ] **Step 2: Define dry-run and pilot stages**

Use 50, then 200, then 500 documents. Require human review for every proposal initially. Freeze app commit, model hashes, sidecar hash, config, and Flow versions for each run.

- [ ] **Step 3: Define acceptance gates**

Track conversion success, exact date support, checker rejection rate, human filename acceptance, description acceptance, flagged-reason accuracy, duplicate correctness, retry count, throughput, and zero silent data loss.

- [ ] **Step 4: Run all available automated validation**

```bash
npm run check
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo test --manifest-path src-tauri/Cargo.toml --all-targets
python -m unittest discover -s sidecar/tests -v
python -m compileall -q sidecar training models
```

- [ ] **Step 5: Perform final branch review**

Review the full branch against the approved design, the Global Constraints, and every Critical/Important finding. Fix findings, rerun the covering tests, then rerun the full suite.

- [ ] **Step 6: Commit**

```bash
git add fixtures docs README.md
git commit -m "docs: add BackLog pilot runbook and fixtures"
```

---

## Completion Criteria

- CI passes on the pull request.
- The trust core is Unicode-safe and all assertions are effective.
- Three identical-content files produce three stable instance IDs, one true content SHA-256, three distinct filenames, and replay-safe manifests.
- Pausing and resuming cannot lose discovered files.
- Sidecar requests time out and recover without hanging the app indefinitely.
- A failed manifest write cannot strand a file in an unreported terminal state.
- Flow 2 uses ManifestId for idempotency and Sha256 for content identity.
- The UI reports backend runtime truth and blocks Start on failed hard prerequisites.
- Model lock verification detects mutation and missing files.
- Windows packaging and pilot execution have explicit, reproducible checklists.
- Any remaining limitation is documented with a concrete validation or deployment dependency rather than presented as completed.